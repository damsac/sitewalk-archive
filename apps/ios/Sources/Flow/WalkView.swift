import SwiftUI

// Live capture — the walk. Everything readable at arm's length; controls
// glove-sized in the thumb zone.

struct WalkView: View {
    @Bindable var model: AppModel
    var scriptedLabel: Bool

    var body: some View {
        VStack(spacing: 0) {
            TimelineView(.periodic(from: .now, by: 1)) { _ in
                RecBanner(timer: model.isPaused ? "PAUSED" : model.elapsedLabel)
            }

            MetaStrip(
                left: model.trade.site,
                right: scriptedLabel ? "DEMO WALK — SCRIPTED" : "REC — SAVED LOCAL",
                warn: true
            )

            ScrollView {
                VStack(alignment: .leading, spacing: 6) {
                    Text(model.transcript)
                        .font(Theme.F.mono(11.5))
                        .foregroundStyle(Theme.C.ink60)
                        .lineSpacing(7)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    Caret(height: 13)
                }
                .padding(.horizontal, Theme.S.screenPad)
                .padding(.vertical, 12)
            }
            .frame(height: 168)
            .defaultScrollAnchor(.bottom)

            SectionHead(left: "CAPTURED", right: "\(model.items.count) ITEMS")
                .overlay(alignment: .top) { Theme.C.ink.frame(height: 1.5) }

            ScrollView {
                VStack(spacing: 0) {
                    ForEach(model.items) { item in
                        CapturedRow(item: item)
                            .transition(.opacity.combined(with: .move(edge: .bottom)))
                    }
                }
            }

            VStack(spacing: 12) {
                if model.isPaused {
                    Button { model.discardWalk() } label: {
                        Text("DISCARD WALK — NOTHING SAVED")
                            .font(Theme.F.mono(10, .semibold))
                            .tracking(1.4)
                            .foregroundStyle(Theme.C.redTag)
                            .frame(height: 30)
                            .frame(maxWidth: .infinity)
                    }
                    .buttonStyle(.plain)
                } else {
                    Waveform()
                }
                HStack(spacing: 9) {
                    Button { model.addPhoto() } label: { PhotoSquareButton() }
                        .buttonStyle(.plain)
                    Button { model.togglePause() } label: {
                        Text(model.isPaused ? "RESUME" : "PAUSE")
                            .font(Theme.F.ui(13.5, .bold))
                            .tracking(1.1)
                            .foregroundStyle(Theme.C.ink)
                            .frame(width: 96)
                            .frame(height: Theme.S.buttonHeight)
                            .overlay(
                                RoundedRectangle(cornerRadius: Theme.S.radius)
                                    .stroke(Theme.C.ink, lineWidth: 2)
                            )
                    }
                    .buttonStyle(.plain)
                    Button { model.finishWalk() } label: { DoneButton() }
                        .buttonStyle(.plain)
                        .frame(maxWidth: .infinity)
                        .disabled(model.items.isEmpty)
                        .opacity(model.items.isEmpty ? 0.4 : 1)
                }
            }
            .padding(.horizontal, Theme.S.screenPad)
            .padding(.top, 10)
            .padding(.bottom, 10)
            .overlay(alignment: .top) { Theme.C.hairline.frame(height: 1) }
        }
        .background(Theme.C.paper.ignoresSafeArea())
        .toolbar(.hidden, for: .navigationBar)
        .navigationBarBackButtonHidden(true)
    }
}
