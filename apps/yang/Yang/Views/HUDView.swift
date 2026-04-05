import SwiftUI

/// Floating stats overlay shown during an active stream.
struct HUDView: View {
    @EnvironmentObject var model: ConnectionModel

    var body: some View {
        HStack(spacing: 14) {
            if case .connecting = model.state {
                ProgressView()
                    .controlSize(.small)
                Text("Connecting…")
                    .foregroundStyle(.secondary)
            } else {
                // Stats row
                statItem(
                    icon: "video.fill",
                    value: String(format: "%.0f fps", model.stats.fps)
                )
                statItem(
                    icon: "antenna.radiowaves.left.and.right",
                    value: model.stats.bitrateMbps > 0
                        ? String(format: "%.1f Mbps", model.stats.bitrateMbps)
                        : "—"
                )
                if model.stats.framesDropped > 0 {
                    statItem(
                        icon: "exclamationmark.triangle",
                        value: "\(model.stats.framesDropped) dropped",
                        color: .orange
                    )
                }

                Divider()
                    .frame(height: 16)

                Button("Disconnect") {
                    model.disconnect()
                }
                .buttonStyle(.plain)
                .foregroundStyle(.red)
                .font(.caption.weight(.medium))
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .fill(.ultraThinMaterial)
        )
        .shadow(radius: 4)
    }

    @ViewBuilder
    private func statItem(icon: String, value: String, color: Color = .primary) -> some View {
        HStack(spacing: 4) {
            Image(systemName: icon)
                .font(.caption)
                .foregroundStyle(.secondary)
            Text(value)
                .font(.caption.monospacedDigit())
                .foregroundStyle(color)
        }
    }
}
