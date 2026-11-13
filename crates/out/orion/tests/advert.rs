//! The discovery record's codec: what a producer writes, a reader reads back, and what it refuses.
//! No Orion here: this runs in the default build.

use std::num::NonZeroU32;

use styx_format::FourCc;
use styx_orion::{Advert, AdvertError, MAX_LEN, MAX_SOCKET, VERSION};

fn advert(socket: &str) -> Advert {
    Advert {
        code: FourCc::GREY,
        width: NonZeroU32::new(640).expect("width"),
        height: NonZeroU32::new(480).expect("height"),
        payload: 640 * 480,
        buffers: 8,
        socket: socket.to_owned(),
    }
}

#[test]
fn a_record_round_trips() {
    let want = advert("/run/styx/front.sock");
    let mut buffer = [0_u8; MAX_LEN];
    let len = want.encode(&mut buffer).expect("encodes");
    assert_eq!(buffer[0], VERSION);
    assert_eq!(Advert::decode(&buffer[..len]).expect("decodes"), want);
}

#[test]
fn a_longest_path_fits_and_one_more_byte_does_not() {
    let mut buffer = [0_u8; MAX_LEN];
    let longest = "p".repeat(MAX_SOCKET);
    let len = advert(&longest).encode(&mut buffer).expect("fits");
    assert_eq!(len, MAX_LEN);
    assert_eq!(
        Advert::decode(&buffer[..len]).expect("decodes").socket,
        longest
    );
    let too_long = "p".repeat(MAX_SOCKET + 1);
    assert_eq!(
        advert(&too_long).encode(&mut buffer),
        Err(AdvertError::Socket)
    );
    assert_eq!(advert("").encode(&mut buffer), Err(AdvertError::Socket));
}

#[test]
fn a_reader_refuses_what_it_does_not_read() {
    let mut buffer = [0_u8; MAX_LEN];
    let len = advert("/run/styx/a.sock")
        .encode(&mut buffer)
        .expect("encodes");
    let mut other_version = buffer;
    other_version[0] = VERSION + 1;
    assert_eq!(
        Advert::decode(&other_version[..len]),
        Err(AdvertError::Version(VERSION + 1))
    );
    assert_eq!(Advert::decode(&buffer[..10]), Err(AdvertError::Malformed));
    assert_eq!(
        Advert::decode(&buffer[..len - 1]),
        Err(AdvertError::Malformed)
    );
    let mut zero_width = buffer;
    zero_width[5..9].copy_from_slice(&0_u32.to_le_bytes());
    assert_eq!(Advert::decode(&zero_width[..len]), Err(AdvertError::Zero));
    let mut bad_utf8 = buffer;
    bad_utf8[23] = 0xff;
    assert_eq!(
        Advert::decode(&bad_utf8[..len]),
        Err(AdvertError::Malformed)
    );
}
