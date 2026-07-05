import SwiftUI
import Observation
import os

// One observable model drives the whole flow:
//   board → walking (pause/resume, photos) → building → review (edit, fill gaps) → sent
// The engine behind it is injected; today that's DemoWalkEngine, tomorrow the
// FFI bridge. The UI never knows the difference.

@MainActor
@Observable
final class AppModel {

    enum Phase: Equatable {
        case board
        case walking
        case building
        case review
    }

    // MARK: State

    var trade: TradeFixture = Fixtures.landscape
    var jobs: [JobFixture] = Fixtures.landscape.jobs
    var phase: Phase = .board
    var path: [Phase] = []

    // Walk state
    var transcript = ""
    var items: [CapturedFixture] = []
    var isPaused = false
    var walkStart = Date()
    var pausedElapsed: TimeInterval = 0
    /// Most-recently-captured item's id, tracked explicitly from the event
    /// loop (Plan 07 D3/Task 10) — under whole-board `boardUpdated` replace,
    /// "array tail == most-recently-captured" is NOT load-bearing: a
    /// re-extraction mints new ids mid-swap and store ordering is insertion
    /// order, not mention order. `addPhoto()` pins by this id, never by
    /// array position. Which item a photo pins to is ultimately a core
    /// concern (HANDOFF open Q3, photo sync schema — Deferred 6); until that
    /// lands, "most-recently-captured id" is the honest interim rule.
    var lastCapturedID: UUID?

    // Review state
    var document: DocumentModel?
    var editingRowID: UUID?
    var editText = ""
    var shareURL: URL?

    private var engine: WalkEngine
    private var source: TranscriptSource?
    private var pumpTask: Task<Void, Never>?
    private var eventTask: Task<Void, Never>?
    private let scripted: Bool

    init(engine: WalkEngine? = nil, scripted: Bool = true) {
        self.engine = engine ?? DemoWalkEngine()
        self.scripted = scripted
    }

    // MARK: Trade switching (validation strategy: same bones, swappable template)

    func switchTrade(_ newTrade: TradeFixture) {
        trade = newTrade
        jobs = newTrade.jobs
    }

    // MARK: Walk lifecycle

    func startWalk() {
        pumpTask?.cancel()
        eventTask?.cancel()

        // begin() is throwing (review P1): the real engine's session start is
        // fallible across FFI. If it fails, the user must NOT enter the
        // walking flow — a dead session would run STT and silently drop every
        // append (capture loss). Stay on the board; walk state untouched.
        // sac: this deserves a visible error surface ("couldn't start the
        // walk — try again"); no error chrome exists in the app yet, so the
        // floor here is the log breadcrumb + not entering .walking. Yours to
        // design.
        let events: AsyncStream<WalkEvent>
        do {
            events = try engine.begin(trade: trade)
        } catch {
            Logger(subsystem: Bundle.main.bundleIdentifier ?? "sitewalk", category: "walk")
                .error("startWalk: engine.begin failed, staying on board: \(error, privacy: .public)")
            return
        }

        transcript = ""
        items = []
        isPaused = false
        walkStart = Date()

        let src: TranscriptSource = scripted ? ScriptedSource(trade: trade) : SpeechSource()
        source = src

        eventTask = Task { [weak self] in
            guard let self else { return }
            for await event in events {
                switch event {
                case .boardUpdated(let items):
                    withAnimation(.easeOut(duration: 0.25)) { self.items = items }
                    // Track the newest by id, NOT array position (see
                    // `lastCapturedID` doc comment).
                    self.lastCapturedID = items.last?.id
                }
            }
        }
        pumpTask = Task { [weak self] in
            guard let self else { return }
            for await chunk in src.chunks {
                self.transcript += chunk
                self.engine.append(transcript: chunk)
            }
        }

        phase = .walking
        path = [.walking]
        src.start()
    }

    func togglePause() {
        isPaused.toggle()
        isPaused ? source?.pause() : source?.resume()
    }

    func addPhoto() {
        guard let id = lastCapturedID,
              let idx = items.firstIndex(where: { $0.id == id }) else { return }
        items[idx].photos += 1
    }

    func discardWalk() {
        source?.stop()
        pumpTask?.cancel()
        eventTask?.cancel()
        transcript = ""
        items = []
        isPaused = false
        phase = .board
        path = []
    }

    func finishWalk() {
        source?.stop()
        phase = .building
        path = [.building]
        Task {
            let doc = await engine.finish()
            self.document = doc
            self.phase = .review
            self.path = [.review]
        }
    }

    var elapsedLabel: String {
        let s = Int(Date().timeIntervalSince(walkStart))
        return String(format: "%02d:%02d", s / 60, s % 60)
    }

    // MARK: Review interactions

    func beginEdit(_ row: DocRowFixture) {
        editingRowID = row.id
        editText = row.amount.hasPrefix("$") ? String(row.amount.dropFirst()) : ""
    }

    func commitEdit() {
        guard let id = editingRowID, var doc = document,
              let index = doc.rows.firstIndex(where: { $0.id == id }) else {
            editingRowID = nil
            return
        }
        let cleaned = editText.replacingOccurrences(of: ",", with: "").trimmingCharacters(in: .whitespaces)
        if let value = Int(cleaned), value > 0 {
            let old = doc.rows[index]
            doc.rows[index] = DocRowFixture(
                title: old.title,
                sub: old.isGap ? "FILLED BY YOU" : old.sub,
                subWarn: false,
                hint: old.hint,
                qty: old.qty,
                amount: "$\(value)",
                isEdit: false,
                isGap: false
            )
            document = doc
        }
        editingRowID = nil
    }

    // MARK: Send

    func makePDF() {
        guard let doc = document else { return }
        shareURL = DocumentPDF.render(trade: trade, document: doc)
    }

    func completeSend() {
        if let index = jobs.firstIndex(where: { !$0.done }) {
            let old = jobs[index]
            jobs[index] = JobFixture(
                time: old.time, name: old.name, sub: old.sub,
                tag: TagFixture(kind: .green, label: "SENT"), done: true
            )
        }
        shareURL = nil
        document = nil
        phase = .board
        path = []
    }
}
