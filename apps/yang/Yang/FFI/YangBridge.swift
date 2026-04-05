import Foundation

/// Global C-callable stats callback trampoline.
///
/// `@convention(c)` closures cannot capture context, so we recover the
/// `ConnectionModel` from the opaque `userdata` pointer that `yang_connect`
/// received and dispatch the update to the main thread.
let yangStatsCallbackGlobal: YangStatsCallback = { statsPtr, userdata in
    guard let statsPtr, let userdata else { return }
    let stats = statsPtr.pointee
    let model = Unmanaged<ConnectionModel>.fromOpaque(userdata).takeUnretainedValue()
    DispatchQueue.main.async { model.handleStats(stats) }
}
