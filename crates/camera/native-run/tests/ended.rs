//! What a session says for each kind of stream end. The simulator cannot provoke a device-failed
//! or consumer-fault end (a device turned off for good needs a driver that fails its bind again and
//! again, which the simulator does not do), so the four kinds are checked here as values: the
//! session returns `Error::StreamEnded` with the kind and the words for it.

use styx_lemnos::StreamEnd;
use styx_native_run::Error;

fn ended(end: StreamEnd) -> Error {
    Error::StreamEnded {
        end,
        text: end.to_string(),
    }
}

#[test]
fn each_stream_end_kind_keeps_its_kind_and_says_its_own_meaning() {
    let cases = [
        (StreamEnd::Closed, "the host ended the frame stream"),
        (StreamEnd::ConsumerFault, "this reader broke the rules"),
        (StreamEnd::DeviceFailed, "the device producing it failed"),
        (StreamEnd::Unknown(77), "kind unknown (end code 77)"),
    ];
    for (end, words) in cases {
        match ended(end) {
            Error::StreamEnded { end: kind, text } => {
                assert_eq!(kind, end);
                assert!(text.contains(words), "{end:?}: {text}");
                assert_eq!(Error::StreamEnded { end: kind, text: text.clone() }.to_string(), text);
            }
            other => panic!("{other:?}"),
        }
    }
    // Unknown is never read as running: it is an end with its own code.
    assert_ne!(StreamEnd::Unknown(0), StreamEnd::Closed);
}
