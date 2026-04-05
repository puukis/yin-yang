import SwiftUI

struct ContentView: View {
    @EnvironmentObject var model: ConnectionModel

    var body: some View {
        switch model.state {
        case .disconnected:
            ConnectionView()
                .environmentObject(model)
        case .connecting, .streaming:
            StreamView()
                .environmentObject(model)
                .ignoresSafeArea()
        case .failed(let message):
            ConnectionView(errorMessage: message)
                .environmentObject(model)
        }
    }
}
