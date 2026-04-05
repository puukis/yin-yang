import AppKit
import Foundation
import QuartzCore

// MARK: - State

enum ConnectionState {
    case disconnected
    case connecting
    case streaming
    case failed(String)
}

// MARK: - Stats

struct StreamStats {
    var fps: Float = 0
    var bitrateMbps: Float = 0
    var framesDecoded: UInt32 = 0
    var framesDropped: UInt32 = 0
    var unrecoverableFrames: UInt32 = 0
}

// MARK: - Model

final class ConnectionModel: ObservableObject {
    @Published var state: ConnectionState = .disconnected

    // Connection settings (persisted via UserDefaults on demand)
    @Published var serverAddress: String = UserDefaults.standard.string(forKey: "serverAddress") ?? "192.168.1.50:9000"
    @Published var maxFps: Int         = UserDefaults.standard.integer(forKey: "maxFps").nonZero ?? 60
    @Published var interpolate: Bool   = UserDefaults.standard.bool(forKey: "interpolate")
    @Published var adaptiveRate: Bool  = UserDefaults.standard.bool(forKey: "adaptiveRate")
    @Published var maxBitrateMbps: Int = UserDefaults.standard.integer(forKey: "maxBitrateMbps")

    @Published var stats: StreamStats = StreamStats()
    @Published var streamSize: CGSize = .zero

    private var session: OpaquePointer? = nil

    // MARK: - Actions

    /// Called by ConnectionView's Connect button.
    func prepareToConnect() {
        guard case .disconnected = state else { return }
        saveSettings()
        state = .connecting
        // StreamView appears; it will call beginConnect(layer:) once its Metal view is ready.
    }

    /// Called from MetalStreamView.makeNSView after the layer is attached.
    func beginConnect(layer: CAMetalLayer) {
        let addr       = serverAddress
        let fps        = UInt8(clamping: maxFps)
        let interp     = interpolate
        let adaptive   = adaptiveRate
        let maxBitrate = UInt32(clamping: maxBitrateMbps)

        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            guard let self else { return }

            let session: OpaquePointer? = addr.withCString { addrPtr in
                var opts           = YangConnectOptions()
                opts.server_addr   = addrPtr
                opts.display_selector = nil
                opts.max_fps       = fps
                opts.min_fps       = min(fps, 30)
                opts.max_bitrate_mbps = maxBitrate
                opts.min_bitrate_mbps = 0
                opts.adaptive_streaming = adaptive
                opts.interpolate   = interp

                let ud  = Unmanaged.passUnretained(self).toOpaque()
                let lp  = Unmanaged.passUnretained(layer).toOpaque()

                return yang_connect(&opts, lp, yangStatsCallbackGlobal, ud)
            }

            DispatchQueue.main.async {
                if let session {
                    self.session = session
                    var w: UInt32 = 0, h: UInt32 = 0
                    yang_stream_size(session, &w, &h)
                    if w > 0 && h > 0 {
                        let scale = layer.contentsScale > 0 ? layer.contentsScale : 2.0
                        var size = CGSize(width:  CGFloat(w) / scale,
                                         height: CGFloat(h) / scale)
                        // Fit within 95% of the available screen area, preserving aspect ratio.
                        if let available = NSScreen.main?.visibleFrame.size {
                            let maxW = available.width  * 0.95
                            let maxH = available.height * 0.95
                            if size.width > maxW || size.height > maxH {
                                let fit = min(maxW / size.width, maxH / size.height)
                                size = CGSize(width:  (size.width  * fit).rounded(),
                                             height: (size.height * fit).rounded())
                            }
                        }
                        self.streamSize = size
                    }
                    self.state = .streaming
                } else {
                    self.state = .failed("Could not connect to \(addr). Check the address and try again.")
                }
            }
        }
    }

    /// Disconnect and return to the connection screen.
    func disconnect() {
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            guard let self else { return }
            if let s = self.session {
                yang_disconnect(s)
                yang_free(s)
                self.session = nil
            }
            DispatchQueue.main.async {
                self.stats      = StreamStats()
                self.streamSize = .zero
                self.state      = .disconnected
            }
        }
    }

    // MARK: - Stats (called from YangBridge callback, already on main thread)

    func handleStats(_ raw: YangStats) {
        stats = StreamStats(
            fps:                raw.fps,
            bitrateMbps:        raw.bitrate_mbps,
            framesDecoded:      raw.frames_decoded,
            framesDropped:      raw.frames_dropped,
            unrecoverableFrames: raw.unrecoverable_frames
        )
    }

    // MARK: - Persistence

    private func saveSettings() {
        UserDefaults.standard.set(serverAddress,   forKey: "serverAddress")
        UserDefaults.standard.set(maxFps,          forKey: "maxFps")
        UserDefaults.standard.set(interpolate,     forKey: "interpolate")
        UserDefaults.standard.set(adaptiveRate,    forKey: "adaptiveRate")
        UserDefaults.standard.set(maxBitrateMbps,  forKey: "maxBitrateMbps")
    }
}

// MARK: - Helpers

private extension Int {
    var nonZero: Int? { self == 0 ? nil : self }
}
