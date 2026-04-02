#[cfg(target_os = "macos")]
pub mod capture;

#[cfg(not(target_os = "macos"))]
pub mod capture {
    use anyhow::Result;
    use crossbeam_channel::Sender;
    use streamd_proto::packets::InputPacket;

    /// No-op input capture used for non-macOS builds.
    pub struct InputCapture;

    impl InputCapture {
        pub fn start(_event_tx: Sender<InputPacket>) -> Result<Self> {
            Ok(Self)
        }

        pub fn release(&self) {}
    }
}
