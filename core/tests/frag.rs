use hysteria_core::{
    frag::{Defragger, frag_udp_message},
    protocol::UDPMessage,
};

fn sample_message(size: usize) -> UDPMessage {
    UDPMessage {
        session_id: 123,
        packet_id: 987,
        frag_id: 0,
        frag_count: 1,
        addr: "example.com:443".into(),
        data: (0..size).map(|i| (i % 251) as u8).collect(),
    }
}

#[test]
fn frag_udp_message_returns_original_when_not_needed() {
    let message = sample_message(32);
    let fragments = frag_udp_message(&message, 256);
    assert_eq!(fragments, vec![message]);
}

#[test]
fn frag_udp_message_splits_payload() {
    let message = sample_message(64);
    let fragments = frag_udp_message(&message, message.header_size() + 20);
    assert_eq!(fragments.len(), 4);
    assert_eq!(fragments[0].frag_id, 0);
    assert_eq!(fragments[3].frag_id, 3);
    assert_eq!(fragments[0].frag_count, 4);
}

#[test]
fn defragger_reassembles_when_all_fragments_arrive() {
    let message = sample_message(64);
    let fragments = frag_udp_message(&message, message.header_size() + 20);

    let mut defragger = Defragger::default();
    assert!(defragger.feed(fragments[1].clone()).is_none());
    assert!(defragger.feed(fragments[0].clone()).is_none());
    assert!(defragger.feed(fragments[3].clone()).is_none());
    let rebuilt = defragger.feed(fragments[2].clone()).unwrap();

    assert_eq!(rebuilt.session_id, message.session_id);
    assert_eq!(rebuilt.packet_id, message.packet_id);
    assert_eq!(rebuilt.addr, message.addr);
    assert_eq!(rebuilt.data, message.data);
    assert_eq!(rebuilt.frag_id, 0);
    assert_eq!(rebuilt.frag_count, 1);
}

#[test]
fn defragger_rejects_invalid_fragment_id() {
    let mut message = sample_message(64);
    message.frag_id = 5;
    message.frag_count = 2;

    let mut defragger = Defragger::default();
    assert!(defragger.feed(message).is_none());
}

#[test]
fn defragger_handles_interleaved_packets() {
    let mut first = sample_message(64);
    first.packet_id = 100;
    let mut second = sample_message(64);
    second.packet_id = 200;
    second.addr = "example.net:443".into();

    let first_fragments = frag_udp_message(&first, first.header_size() + 20);
    let second_fragments = frag_udp_message(&second, second.header_size() + 20);

    let mut defragger = Defragger::default();
    assert!(defragger.feed(first_fragments[0].clone()).is_none());
    assert!(defragger.feed(second_fragments[0].clone()).is_none());
    assert!(defragger.feed(first_fragments[1].clone()).is_none());
    assert!(defragger.feed(second_fragments[1].clone()).is_none());
    assert!(defragger.feed(first_fragments[2].clone()).is_none());
    assert!(defragger.feed(second_fragments[2].clone()).is_none());

    let rebuilt_first = defragger.feed(first_fragments[3].clone()).unwrap();
    let rebuilt_second = defragger.feed(second_fragments[3].clone()).unwrap();

    assert_eq!(rebuilt_first.addr, first.addr);
    assert_eq!(rebuilt_first.data, first.data);
    assert_eq!(rebuilt_second.addr, second.addr);
    assert_eq!(rebuilt_second.data, second.data);
}
