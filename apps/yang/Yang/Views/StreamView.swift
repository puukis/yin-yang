import SwiftUI
import QuartzCore

// MARK: - Metal host view

/// An NSView backed by a CAMetalLayer. Rust renders into this layer.
class MetalHostView: NSView {
    let metalLayer = CAMetalLayer()

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layerContentsRedrawPolicy = .never
        metalLayer.contentsScale = NSScreen.main?.backingScaleFactor ?? 2.0
        metalLayer.backgroundColor = CGColor(red: 0, green: 0, blue: 0, alpha: 1)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func makeBackingLayer() -> CALayer { metalLayer }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        if let scale = window?.backingScaleFactor {
            metalLayer.contentsScale = scale
        }
    }
}

// MARK: - NSViewRepresentable wrapper

struct MetalStreamView: NSViewRepresentable {
    @EnvironmentObject var model: ConnectionModel

    func makeNSView(context: Context) -> MetalHostView {
        let view = MetalHostView()
        // Trigger connect on the next run-loop pass so the layer is attached
        // to the view hierarchy before we hand its pointer to Rust.
        DispatchQueue.main.async {
            self.model.beginConnect(layer: view.metalLayer)
        }
        return view
    }

    func updateNSView(_ nsView: MetalHostView, context: Context) {}

    static func dismantleNSView(_ nsView: MetalHostView, coordinator: ()) {}
}

// MARK: - StreamView

struct StreamView: View {
    @EnvironmentObject var model: ConnectionModel

    var body: some View {
        ZStack {
            MetalStreamView()
                .frame(maxWidth: .infinity, maxHeight: .infinity)

            VStack {
                Spacer()
                HStack {
                    Spacer()
                    HUDView()
                        .padding(12)
                }
            }
        }
        .background(Color.black)
        .onDisappear { model.disconnect() }
        .onChange(of: model.streamSize) { size in
            guard size != .zero else { return }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
                guard let window = NSApplication.shared.windows.first(where: { $0.isKeyWindow })
                                ?? NSApplication.shared.windows.first
                else { return }
                window.setContentSize(size)
                window.center()
            }
        }
    }
}
