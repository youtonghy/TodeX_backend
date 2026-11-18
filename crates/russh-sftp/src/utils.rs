use bytes::Bytes;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::error::Error;

pub(crate) fn unix(time: SystemTime) -> u32 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_secs() as u32,
        Err(before_epoch) => {
            let elapsed = before_epoch.duration();
            // Floor to a whole second, then retain the low 32 bits as on the wire.
            (elapsed.as_secs() as u32)
                .wrapping_add(u32::from(elapsed.subsec_nanos() != 0))
                .wrapping_neg()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn unix_floors_fractional_seconds_and_wraps_to_wire_width() {
        let fraction = Duration::from_millis(500);
        assert_eq!(unix(UNIX_EPOCH + fraction), 0);
        assert_eq!(unix(UNIX_EPOCH - fraction), u32::MAX);
        assert_eq!(unix(UNIX_EPOCH - Duration::from_secs(1)), u32::MAX);
        assert_eq!(unix(UNIX_EPOCH + Duration::from_secs(1 << 32)), 0);
    }
}

pub(crate) async fn read_packet<S: AsyncRead + Unpin>(
    stream: &mut S,
    max_length: u32,
) -> Result<Bytes, Error> {
    let length = stream.read_u32().await?;
    if length > max_length {
        return Err(Error::BadMessage("packet length limit exceeded".to_owned()));
    }

    let mut buf = vec![0; length as usize];
    stream.read_exact(&mut buf).await?;

    Ok(Bytes::from(buf))
}
