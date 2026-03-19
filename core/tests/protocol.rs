use std::io::Cursor;

use http::HeaderMap;
use hysteria_core::protocol::{
    AuthRequest, AuthResponse, COMMON_HEADER_PADDING, FRAME_TYPE_TCP_REQUEST, STATUS_AUTH_OK,
    UDPMessage, auth_request_from_headers, auth_request_to_headers, auth_response_from_headers,
    auth_response_to_headers, parse_udp_message, read_tcp_request, read_tcp_response,
    write_tcp_request, write_tcp_response,
};

#[test]
fn udp_message_buffer_too_small_returns_none() {
    let message = UDPMessage {
        session_id: 66,
        packet_id: 77,
        frag_id: 2,
        frag_count: 5,
        addr: "random_addr".into(),
        data: b"random_data".to_vec(),
    };
    let mut buf = [0_u8; 20];
    assert_eq!(message.serialize(&mut buf), None);
}

#[test]
fn udp_message_wire_format_matches_go_layout() {
    let message = UDPMessage {
        session_id: 1,
        packet_id: 1,
        frag_id: 0,
        frag_count: 1,
        addr: "example.com:80".into(),
        data: b"GET /nothing HTTP/1.1\r\n".to_vec(),
    };
    let expected = vec![
        0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x0e, 0x65, 0x78, 0x61, 0x6d, 0x70, 0x6c,
        0x65, 0x2e, 0x63, 0x6f, 0x6d, 0x3a, 0x38, 0x30, 0x47, 0x45, 0x54, 0x20, 0x2f, 0x6e, 0x6f,
        0x74, 0x68, 0x69, 0x6e, 0x67, 0x20, 0x48, 0x54, 0x54, 0x50, 0x2f, 0x31, 0x2e, 0x31, 0x0d,
        0x0a,
    ];

    let mut buf = vec![0_u8; 4096];
    let n = message.serialize(&mut buf).unwrap();
    assert_eq!(&buf[..n], expected.as_slice());
    assert_eq!(parse_udp_message(&expected).unwrap(), message);
}

#[test]
fn udp_message_long_roundtrip_works() {
    let addr = format!("{}:9000", "goofy_".repeat(70));
    let message = UDPMessage {
        session_id: 1_329_655_244,
        packet_id: 62_233,
        frag_id: 8,
        frag_count: 19,
        addr,
        data: b"God is great, beer is good, and people are crazy.".to_vec(),
    };

    let mut buf = vec![0_u8; 4096];
    let n = message.serialize(&mut buf).unwrap();
    let parsed = parse_udp_message(&buf[..n]).unwrap();
    assert_eq!(parsed, message);
}

#[test]
fn malformed_udp_messages_fail() {
    let cases = [
        &b""[..],
        &b"\0\0\0\0"[..],
        &b"\0\0\0\0\0\0\0\0\0\0\0\0"[..],
        &b"\x66\xcc\xff\xff\x11\x22\x33\x44\x55"[..],
        &b"\x66\xcc\xff\xff\x11\x22\x33\x44\x90\xaa\xbb\xcc\xdd\xee\xff"[..],
    ];

    for case in cases {
        assert!(parse_udp_message(case).is_err());
    }
}

#[test]
fn tcp_request_read_matches_go_behavior() {
    let cases = [
        (&b"\x0egoogle.com:443\x00"[..], "google.com:443"),
        (&b"\x0bholy.cc:443\x02gg"[..], "holy.cc:443"),
    ];

    for (data, expected) in cases {
        let mut cursor = Cursor::new(data);
        assert_eq!(read_tcp_request(&mut cursor).unwrap(), expected);
    }

    assert!(read_tcp_request(&mut Cursor::new(&b"\x0bhoho"[..])).is_err());
    assert!(read_tcp_request(&mut Cursor::new(&b"\x0bholy.cc:443\x05x"[..])).is_err());
}

#[test]
fn tcp_request_write_has_expected_prefix() {
    let mut buf = Vec::new();
    write_tcp_request(&mut buf, "google.com:443").unwrap();

    let expected_prefix = [
        ((FRAME_TYPE_TCP_REQUEST >> 8) as u8) | 0x40,
        FRAME_TYPE_TCP_REQUEST as u8,
        0x0e,
    ];
    assert!(buf.starts_with(&expected_prefix));
    assert!(buf.len() > expected_prefix.len() + "google.com:443".len());
}

#[test]
fn tcp_response_read_matches_go_behavior() {
    let cases = [
        (&b"\x00\x0bhello world\x00"[..], (true, "hello world")),
        (&b"\x01\x06stop!!\x05xxxxx"[..], (false, "stop!!")),
        (&b"\x01\x00\x05xxxxx"[..], (false, "")),
    ];

    for (data, expected) in cases {
        let mut cursor = Cursor::new(data);
        let got = read_tcp_response(&mut cursor).unwrap();
        assert_eq!(got.0, expected.0);
        assert_eq!(got.1, expected.1);
    }

    assert!(read_tcp_response(&mut Cursor::new(&b"\x00\x0bhoho"[..])).is_err());
    assert!(read_tcp_response(&mut Cursor::new(&b"\x01\x05jesus\x05x"[..])).is_err());
}

#[test]
fn tcp_response_write_has_expected_prefix() {
    let mut buf = Vec::new();
    write_tcp_response(&mut buf, true, "Connected").unwrap();
    assert_eq!(buf[0], 0);
    assert_eq!(buf[1], 9);
    assert!(buf[2..].starts_with(b"Connected"));
}

#[test]
fn auth_header_roundtrip_matches_spec() {
    let mut headers = HeaderMap::new();
    let request = AuthRequest {
        auth: "hunter2".into(),
        rx: 123_456,
    };
    auth_request_to_headers(&mut headers, &request).unwrap();
    let parsed = auth_request_from_headers(&headers);
    assert_eq!(parsed, request);
    assert!(headers.contains_key(COMMON_HEADER_PADDING));

    let mut response_headers = HeaderMap::new();
    let response = AuthResponse {
        udp_enabled: true,
        rx: 0,
        rx_auto: true,
    };
    auth_response_to_headers(&mut response_headers, &response).unwrap();
    let parsed_response = auth_response_from_headers(&response_headers);
    assert_eq!(parsed_response, response);
    assert_eq!(STATUS_AUTH_OK, 233);
}
