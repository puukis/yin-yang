import SwiftUI

struct ConnectionView: View {
    @EnvironmentObject var model: ConnectionModel

    var errorMessage: String? = nil

    var body: some View {
        VStack(spacing: 0) {
            // ── Header ────────────────────────────────────────────────────
            VStack(spacing: 6) {
                Text("☯")
                    .font(.system(size: 48))
                Text("Yang")
                    .font(.system(size: 22, weight: .semibold))
                Text("Remote Desktop Client")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            }
            .padding(.top, 28)
            .padding(.bottom, 20)

            Divider()

            // ── Form ──────────────────────────────────────────────────────
            Form {
                Section("Server") {
                    TextField("Address", text: $model.serverAddress)
                        .font(.system(.body, design: .monospaced))
                }

                Section("Stream") {
                    HStack {
                        Text("Max FPS")
                        Spacer()
                        Picker("", selection: $model.maxFps) {
                            ForEach([30, 60, 90, 120], id: \.self) { fps in
                                Text("\(fps) fps").tag(fps)
                            }
                        }
                        .pickerStyle(.menu)
                        .labelsHidden()
                        .frame(width: 90)
                    }

                    HStack {
                        Text("Max Bitrate")
                        Spacer()
                        TextField("0", value: $model.maxBitrateMbps, formatter: NumberFormatter())
                            .multilineTextAlignment(.trailing)
                            .frame(width: 52)
                        Text("Mbps")
                            .foregroundStyle(.secondary)
                    }

                    Toggle("GPU Interpolation", isOn: $model.interpolate)
                    Toggle("Adaptive Rate",     isOn: $model.adaptiveRate)
                }
            }
            .formStyle(.grouped)
            .scrollDisabled(true)

            // ── Error banner ──────────────────────────────────────────────
            if let msg = errorMessage {
                HStack(spacing: 6) {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .foregroundStyle(.orange)
                    Text(msg)
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 6)
            }

            // ── Connect button ────────────────────────────────────────────
            Button(action: { model.prepareToConnect() }) {
                Text("Connect")
                    .frame(maxWidth: .infinity)
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.large)
            .disabled(model.serverAddress.trimmingCharacters(in: .whitespaces).isEmpty)
            .padding(.horizontal, 20)
            .padding(.bottom, 24)
        }
        .frame(width: 400)
    }
}
