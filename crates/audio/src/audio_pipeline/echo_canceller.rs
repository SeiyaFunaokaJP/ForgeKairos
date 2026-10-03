// .forgekairos-docs/DESIGN.md §4.6 (ADR-F06): the real canceller is LiveKit's `libwebrtc` APM,
// whose prebuilt `webrtc-sys` library is what the fork drops from the build. Without calls the
// microphone and the speakers never run together, so every target uses the no-op canceller
// upstream keeps for windows-gnu and FreeBSD. `cfg(any())` switches the real one off instead of
// deleting it, so upstream edits to it still merge.
#[cfg(any())]
#[cfg(not(any(all(target_os = "windows", target_env = "gnu"), target_os = "freebsd")))]
mod real_implementation {
    use anyhow::Context;
    use libwebrtc::native::apm;
    use parking_lot::Mutex;
    use std::sync::Arc;

    use crate::{CHANNEL_COUNT, SAMPLE_RATE};

    #[derive(Clone)]
    pub struct EchoCanceller(Arc<Mutex<apm::AudioProcessingModule>>);

    impl Default for EchoCanceller {
        fn default() -> Self {
            // Sound-effect playback only feeds this APM through `process_reverse_stream`
            // for AEC reference; gain/HPF/NS would be no-ops here, so we keep the
            // original (echo only) configuration via the legacy flag form.
            Self(Arc::new(Mutex::new(
                apm::AudioProcessingModule::from_flags(true, false, false, false),
            )))
        }
    }

    impl EchoCanceller {
        pub fn process_reverse_stream(&mut self, buf: &mut [i16]) {
            self.0
                .lock()
                .process_reverse_stream(buf, SAMPLE_RATE.get() as i32, CHANNEL_COUNT.get().into())
                .expect("Audio input and output threads should not panic");
        }

        pub fn process_stream(&mut self, buf: &mut [i16]) -> anyhow::Result<()> {
            self.0
                .lock()
                .process_stream(buf, SAMPLE_RATE.get() as i32, CHANNEL_COUNT.get() as i32)
                .context("livekit audio processor error")
        }
    }
}

mod fake_implementation {
    #[derive(Clone, Default)]
    pub struct EchoCanceller;

    impl EchoCanceller {
        pub fn process_reverse_stream(&mut self, _buf: &mut [i16]) {}
        pub fn process_stream(&mut self, _buf: &mut [i16]) -> anyhow::Result<()> {
            Ok(())
        }
    }
}

pub use fake_implementation::EchoCanceller;
#[cfg(any())]
#[cfg(not(any(all(target_os = "windows", target_env = "gnu"), target_os = "freebsd")))]
pub use real_implementation::EchoCanceller;
