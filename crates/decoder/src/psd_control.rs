use mygo_core::{Cancel, Config};
use std::{
    io,
    time::{Duration, Instant},
};

pub(super) struct Control {
    cancel: Cancel,
    deadline: Instant,
}

impl Control {
    pub fn new(config: &Config, cancel: &Cancel) -> Self {
        Self {
            cancel: cancel.clone(),
            deadline: Instant::now() + Duration::from_millis(config.decode_timeout_ms),
        }
    }

    pub fn check(&self) -> io::Result<()> {
        if self.cancel.cancelled() {
            Err(io::Error::other("PSD: decode cancelled"))
        } else if Instant::now() >= self.deadline {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "PSD: decode timed out",
            ))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_is_checked_without_reading_more_input() {
        let mut control = Control::new(&Config::default(), &Cancel::default());
        control.deadline = Instant::now();
        assert_eq!(control.check().unwrap_err().kind(), io::ErrorKind::TimedOut);
    }
}
