import SwiftUI

@main
struct YangApp: App {
    @StateObject private var model = ConnectionModel()

    var body: some Scene {
        WindowGroup("Yang") {
            ContentView()
                .environmentObject(model)
                .frame(minWidth: 480, minHeight: 340)
        }
        .windowResizability(.contentSize)
    }
}
