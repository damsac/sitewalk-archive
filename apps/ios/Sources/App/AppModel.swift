import SwiftUI
import Observation

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
        transcript = ""
        items = []
        isPaused = false
        walkStart = Date()
        engine.begin(trade: trade)

        let src: TranscriptSource = scripted ? ScriptedSource(trade: trade) : SpeechSource()
        source = src

        eventTask = Task { [weak self] in
            guard let self else { return }
            for await event in self.engine.events {
                switch event {
                case .itemCaptured(let item):
                    withAnimation(.easeOut(duration: 0.25)) { self.items.append(item) }
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
        guard let lastIndex = items.indices.last else { return }
        items[lastIndex].photos += 1
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
