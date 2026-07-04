import Foundation

// The real bridge adapter (Plan 07 Task 11): MurmurEngine: WalkEngine puts
// murmur-core behind sac's app via the `crates/ffi` UniFFI bridge. Formatting
// layer lives here (D2) — core emits display-copy-free structured data
// (cents, unix seconds, integer doc number, label keys); this file formats
// currency/date/prefix and owns letterhead/board-chrome lookups.
//
// WALL (Task 9, honestly reported — see docs/plans/2026-07-04-rust-core-07-ffi-bridge.md
// deviation notes / the FFI-bridge landing report): this project's Nix dev
// shell (flake.nix) provides only the HOST rustc/cargo — no rustup, no
// multi-target rust-overlay/fenix. `cargo build --target aarch64-apple-ios-sim`
// fails with E0463 ("can't find crate for `core`/`std`") because that
// target's std isn't installed and there's no way to add it from this shell.
// The `crates/ffi` crate itself was proven correct: built for the HOST
// target and run through `cargo run -p ffi --features uniffi-bindgen-cli
// --bin uniffi-bindgen -- generate --library target/release/libffi.dylib
// --language swift` successfully, producing the full expected Swift surface
// (MurmurEngine, WalkSession, EngineConfig, DocumentPayload, DocLine,
// BoardItem, WalkEvent, WalkEventListener). What's still missing is an
// iOS-linkable static lib/xcframework and the generated `MurmurCoreFFI`
// Swift package to link it — that needs the iOS cross-compilation toolchain
// (rustup + `rustup target add aarch64-apple-ios-sim x86_64-apple-ios-sim`,
// or a rust-overlay/fenix toolchain in flake.nix with those targets) added to
// this project's dev shell, which is out of this task's authorized scope
// (apps/ios only).
//
// This file is written to the real generated Swift API (field/method names
// verified against the host-built bindings above) so it activates the
// moment `import MurmurCoreFFI` resolves — no further edits should be
// needed once Task 9's toolchain gap is closed. Until then `canImport`
// keeps it inert so the app keeps building on DemoWalkEngine (D10).
#if canImport(MurmurCoreFFI)
import MurmurCoreFFI

private typealias FFIMurmurEngine = MurmurCoreFFI.MurmurEngine
private typealias FFIWalkSession = MurmurCoreFFI.WalkSession
private typealias FFIEngineConfig = MurmurCoreFFI.EngineConfig
private typealias FFIDocumentPayload = MurmurCoreFFI.DocumentPayload
private typealias FFIDocLine = MurmurCoreFFI.DocLine
private typealias FFIBoardItem = MurmurCoreFFI.BoardItem
private typealias FFIWalkEvent = MurmurCoreFFI.WalkEvent
private typealias FFIWalkEventListener = MurmurCoreFFI.WalkEventListener

/// Bridges a Rust callback (`WalkEventListener.onEvent`, invoked off-main) to
/// the `AsyncStream` continuation on `@MainActor` (D3/Self-Review: ordering +
/// per-session stream lifetime).
private final class BoardListener: FFIWalkEventListener {
    private let onBoardUpdated: @Sendable ([FFIBoardItem]) -> Void
    init(onBoardUpdated: @escaping @Sendable ([FFIBoardItem]) -> Void) {
        self.onBoardUpdated = onBoardUpdated
    }
    func onEvent(event: FFIWalkEvent) {
        switch event {
        case .boardUpdated(let items):
            onBoardUpdated(items)
        }
    }
}

@MainActor
final class MurmurEngine: WalkEngine {
    private let engine: FFIMurmurEngine
    private var session: FFIWalkSession?
    private var continuation: AsyncStream<WalkEvent>.Continuation?

    init(config: FFIEngineConfig) {
        self.engine = FFIMurmurEngine(config: config)
    }

    func begin(trade: TradeFixture) -> AsyncStream<WalkEvent> {
        // A second begin() cancels the first stream cleanly (Self-Review:
        // per-session stream lifetime) — finish the old continuation before
        // handing out a fresh one.
        continuation?.finish()
        let (stream, cont) = AsyncStream<WalkEvent>.makeStream()
        continuation = cont

        let newSession = engine.beginWalk(jobId: nil, template: trade.key) // template key = trade.key (D4)
        newSession.setEventListener(listener: BoardListener { [weak self] items in
            // Rust callback → hop to main → yield (events on main, D3).
            Task { @MainActor in
                self?.continuation?.yield(.boardUpdated(items.map(Self.board)))
            }
        })
        session = newSession
        return stream
    }

    func append(transcript: String) {
        session?.appendTranscript(text: transcript)
    }

    func finish() async -> DocumentModel {
        continuation?.finish()
        continuation = nil
        guard let session else { return Self.emptyDocument() }
        let payload = await session.finish()
        return Self.document(payload)
    }

    // MARK: - Formatting layer (D2): core is display-copy-free; this is
    // where cents → "$285", doc_number → "EST-0047", job_date_unix →
    // "JUL 01 2026", and label keys → display copy happen.

    private static func board(_ item: FFIBoardItem) -> CapturedFixture {
        CapturedFixture(
            id: UUID(uuidString: item.id) ?? UUID(),
            tag: tag(for: item.kind),
            text: item.text,
            right: item.right,
            photos: Int(item.photoCount)
        )
    }

    private static func tag(for kind: String) -> TagFixture {
        switch kind {
        case "safety": return TagFixture(kind: .red, label: "SAFETY")
        case "price": return TagFixture(kind: .green, label: "PRICE")
        case "part": return TagFixture(kind: .yellow, label: "PART")
        case "decision": return TagFixture(kind: .plain, label: "DECISION")
        default: return TagFixture(kind: .plain, label: "ITEM")
        }
    }

    private static let centsFormatter: NumberFormatter = {
        let f = NumberFormatter()
        f.numberStyle = .decimal
        return f
    }()

    private static func amountString(_ cents: Int64?) -> String {
        guard let cents else { return "——" }
        let dollars = Double(cents) / 100.0
        return "$" + (centsFormatter.string(from: NSNumber(value: dollars)) ?? "\(dollars)")
    }

    private static let dateFormatter: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "MMM dd yyyy"
        f.locale = Locale(identifier: "en_US_POSIX")
        return f
    }()

    private static func dateLabel(_ unixSeconds: UInt64) -> String {
        dateFormatter.string(from: Date(timeIntervalSince1970: TimeInterval(unixSeconds))).uppercased()
    }

    private static func docNumberLabel(docKind: String, docNumber: UInt64) -> String {
        let prefix: String
        switch docKind {
        case "estimate": prefix = "EST"
        case "inspection": prefix = "IR"
        default: prefix = "MO"
        }
        return "\(prefix)-\(String(format: "%04d", docNumber))"
    }

    /// Per-`doc_kind` display copy the milestone doesn't yet source from
    /// core — letterhead/board chrome stays in `TradeFixture` (D2); this
    /// table is the document-body chrome only (total label, footer note,
    /// send button copy).
    private static func totalLabel(_ key: String) -> String {
        switch key {
        case "deposit_deduction": return "DEPOSIT DEDUCTION"
        case "findings": return "FINDINGS"
        default: return "TOTAL"
        }
    }

    private static func note(for docKind: String, queued: Bool) -> String {
        if queued {
            return "SAVED OFFLINE — WILL FINISH WHEN YOU RECONNECT"
        }
        switch docKind {
        case "inspection": return "FINDINGS MARKED — NOT YET ASSESSED"
        case "report": return "DEDUCTIONS LEFT OPEN ARE MARKED — CONFIRM BEFORE SENDING"
        default: return "GAPS ARE MARKED — TAP TO FILL BEFORE SENDING"
        }
    }

    private static func sendLabel(for docKind: String) -> String {
        switch docKind {
        case "inspection": return "SEND REPORT"
        case "report": return "SEND REPORT"
        default: return "SEND ESTIMATE"
        }
    }

    private static func row(_ line: FFIDocLine) -> DocRowFixture {
        DocRowFixture(
            title: line.title,
            sub: line.isGap ? "NOT HEARD — TAP OR SAY IT" : line.detail,
            subWarn: line.isGap,
            hint: nil, // Deferred 4: price-book autofill hint
            qty: line.qty,
            amount: amountString(line.amountCents),
            isEdit: false, // Deferred 4: pre-filled-from-history affordance
            isGap: line.isGap
        )
    }

    private static func document(_ payload: FFIDocumentPayload) -> DocumentModel {
        DocumentModel(
            rows: payload.lines.map(row),
            totalKey: totalLabel(payload.totalLabelKey),
            staticTotal: payload.staticTotalCents.map(amountString) ?? "——",
            note: note(for: payload.docKind, queued: payload.queued),
            send: sendLabel(for: payload.docKind)
        )
    }

    private static func emptyDocument() -> DocumentModel {
        DocumentModel(rows: [], totalKey: "TOTAL", staticTotal: "——", note: "", send: "SEND")
    }
}
#endif
