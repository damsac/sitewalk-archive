import SwiftUI
import os
#if canImport(MurmurCoreFFI)
import MurmurCoreFFI
#endif

// Key-free breadcrumb so you can confirm from the console which engine is live
// (real murmur-core vs. the scripted demo) without ever logging the API key.
private let engineLog = Logger(subsystem: "com.damsac.sitewalk", category: "engine")

// Default launch: the real app flow (board → walk → build → review → sent),
// running on DemoWalkEngine + ScriptedSource until the FFI bridge lands.
//
// Launch arguments (used by design QA and screenshot automation):
//   screen=components|jobs|capture|document  → static design gallery pages
//   live=1                                   → real mic + on-device STT
//   autoflow=1                               → auto-starts a scripted walk and
//                                              auto-finishes it (state machine demo)
//   demo=1                                   → force DemoWalkEngine even if a
//                                              key is configured (D10)

/// Engine selection (Plan 07 D10/Task 11): real `MurmurEngine` when an API
/// key + config resolve, `DemoWalkEngine` when launched with `demo=1` OR
/// when no key is configured — so the design gallery and scripted autoflow
/// demos still run with zero backend. `nil` here means "use AppModel's
/// DemoWalkEngine default." Delete nothing (D10) — every existing launch arg
/// keeps working.
@MainActor
private func resolveEngine(demo: Bool) -> WalkEngine? {
    if demo {
        engineLog.notice("engine=demo (forced via demo=1)")
        return nil
    }
    #if canImport(MurmurCoreFFI)
    guard
        let apiKey = Bundle.main.object(forInfoDictionaryKey: "PPQ_API_KEY") as? String,
        !apiKey.isEmpty
    else {
        engineLog.notice("engine=demo (no PPQ_API_KEY configured)")
        return nil // no key configured -> demo path (D10)
    }
    let baseURL = ProcessInfo.processInfo.environment["ANTHROPIC_BASE_URL"]
    // iOS does not pre-create Application Support; murmur-core opens its SQLite
    // store at dbPath and panics if the parent directory is missing. Ensure it
    // exists before handing the path to the engine.
    let appSupport = FileManager.default
        .urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
    try? FileManager.default.createDirectory(at: appSupport, withIntermediateDirectories: true)
    let dbPath = appSupport
        .appendingPathComponent("murmur.sqlite3")
        .path
    let config = EngineConfig(
        dbPath: dbPath,
        deviceId: UIDevice.current.identifierForVendor?.uuidString ?? "unknown-device",
        apiKey: apiKey,
        baseUrl: (baseURL?.isEmpty ?? true) ? nil : baseURL,
        modelLive: "claude-haiku-4-5",
        modelProcessing: "claude-sonnet-4-5",
        modelReflection: "claude-haiku-4-5"
    )
    engineLog.notice("engine=real (murmur-core MurmurEngine, key len=\(apiKey.count, privacy: .public))")
    // Throwing constructor (no panics across FFI): if the store can't open,
    // fall back to the demo path rather than crash at launch (D10). The
    // Application Support dir is created above, before this fallible init, so a
    // missing dir can't silently demote a real-key launch to the demo engine.
    return try? MurmurEngine(config: config)
    #else
    engineLog.notice("engine=demo (MurmurCoreFFI not linked)")
    return nil
    #endif
}

@main
struct GalleryApp: App {
    var body: some Scene {
        WindowGroup {
            RootRouter()
        }
    }
}

struct RootRouter: View {
    private static let args = ProcessInfo.processInfo.arguments

    var body: some View {
        if Self.args.contains(where: { $0.hasPrefix("screen=") }) {
            GalleryRoot()
        } else {
            AppRoot(
                live: Self.args.contains("live=1"),
                demo: Self.args.contains("demo=1"),
                autoflowRounds: Self.args
                    .first(where: { $0.hasPrefix("autoflow=") })
                    .flatMap { Int($0.dropFirst("autoflow=".count)) } ?? 0
            )
        }
    }
}

struct AppRoot: View {
    @State private var model: AppModel
    private let live: Bool
    private let autoflowRounds: Int

    @MainActor
    init(live: Bool, demo: Bool, autoflowRounds: Int) {
        self.live = live
        self.autoflowRounds = autoflowRounds
        _model = State(initialValue: AppModel(engine: resolveEngine(demo: demo), scripted: !live))
    }

    var body: some View {
        NavigationStack(path: Bindable(model).path) {
            BoardView(model: model)
                .navigationDestination(for: AppModel.Phase.self) { phase in
                    switch phase {
                    case .walking:
                        WalkView(model: model, scriptedLabel: !live)
                    case .building:
                        BuildView(model: model)
                    case .review:
                        ReviewView(model: model)
                    case .board:
                        BoardView(model: model)
                    }
                }
        }
        .tint(Theme.C.ink)
        .task {
            if live {
                _ = await SpeechSource.requestPermissions()
            }
            for round in 0..<autoflowRounds {
                if round > 0 {
                    model.completeSend()
                    try? await Task.sleep(for: .seconds(1))
                }
                try? await Task.sleep(for: .seconds(1))
                model.startWalk()
                // Let the scripted walk play out, then finish it.
                try? await Task.sleep(for: .seconds(16))
                if model.phase == .walking {
                    model.finishWalk()
                }
                try? await Task.sleep(for: .seconds(3))
            }
            // Screenshot-automation hook: render the PDF unattended.
            if autoflowRounds > 0, ProcessInfo.processInfo.arguments.contains("autopdf=1") {
                try? await Task.sleep(for: .seconds(1))
                if model.phase == .review {
                    model.makePDF()
                }
            }
        }
    }
}

// MARK: - Static design gallery (kept for design QA and previews)

struct GalleryRoot: View {
    enum Dest: String, Hashable, CaseIterable {
        case components, jobs, capture, document

        var title: String {
            switch self {
            case .components: return "COMPONENT KIT"
            case .jobs: return "01 · JOBS BOARD"
            case .capture: return "02 · CAPTURE"
            case .document: return "04 · DOCUMENT REVIEW"
            }
        }
    }

    static func initialPath() -> [Dest] {
        if let arg = ProcessInfo.processInfo.arguments.first(where: { $0.hasPrefix("screen=") }),
           let dest = Dest(rawValue: String(arg.dropFirst("screen=".count))) {
            return [dest]
        }
        return []
    }

    @State private var path: [Dest] = GalleryRoot.initialPath()

    var body: some View {
        NavigationStack(path: $path) {
            VStack(alignment: .leading, spacing: 0) {
                VStack(alignment: .leading, spacing: 6) {
                    HStack(spacing: 10) {
                        Rectangle().fill(Theme.C.orange).frame(width: 13, height: 13)
                        Text("SITEWALK")
                            .font(Theme.F.ui(24, .extraBold))
                            .tracking(3.5)
                    }
                    Text("Design system gallery — DS-01")
                        .font(Theme.F.mono(9))
                        .foregroundStyle(Theme.C.ink60)
                }
                .padding(.horizontal, Theme.S.screenPad)
                .padding(.top, 18)
                .padding(.bottom, 16)
                .overlay(alignment: .bottom) { Theme.C.ink.frame(height: 2) }

                ForEach(Dest.allCases, id: \.self) { dest in
                    NavigationLink(value: dest) {
                        HStack {
                            Text(dest.title)
                                .font(Theme.F.mono(11, .medium))
                                .tracking(1.2)
                                .foregroundStyle(Theme.C.ink)
                            Spacer()
                            Text("→")
                                .font(Theme.F.mono(11))
                                .foregroundStyle(Theme.C.orangeDeep)
                        }
                        .padding(.horizontal, Theme.S.screenPad)
                        .padding(.vertical, 16)
                        .overlay(alignment: .bottom) { Theme.C.hairline.frame(height: 1) }
                    }
                }

                Spacer()
            }
            .background(Theme.C.paper.ignoresSafeArea())
            .navigationDestination(for: Dest.self) { dest in
                switch dest {
                case .components: ComponentsPage()
                case .jobs: JobsBoardScreen(trade: Fixtures.landscape)
                case .capture: CaptureScreen(trade: Fixtures.landscape)
                case .document: DocumentReviewScreen(trade: Fixtures.landscape)
                }
            }
        }
        .tint(Theme.C.ink)
    }
}
