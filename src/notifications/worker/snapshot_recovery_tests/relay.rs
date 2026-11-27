use openssl::{
    asn1::Asn1Time,
    bn::BigNum,
    ec::{EcGroup, EcKey},
    hash::MessageDigest,
    nid::Nid,
    pkey::PKey,
    ssl::{SslAcceptor, SslMethod},
    x509::{
        X509, X509NameBuilder,
        extension::{BasicConstraints, SubjectAlternativeName},
    },
};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::Path,
    time::Duration,
};
use tokio::net::TcpStream;

pub(super) const SENDER: &str = "candidate@example.invalid";
pub(super) const RECIPIENT: &str = "owner@example.invalid";

pub(super) fn acceptor(certificate: &Path) -> SslAcceptor {
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
    let key = PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", "web test SMTP relay")
        .unwrap();
    let name = name.build();
    let mut cert = X509::builder().unwrap();
    cert.set_version(2).unwrap();
    cert.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    cert.set_subject_name(&name).unwrap();
    cert.set_issuer_name(&name).unwrap();
    cert.set_pubkey(&key).unwrap();
    cert.set_not_before(&Asn1Time::days_from_now(0).unwrap())
        .unwrap();
    cert.set_not_after(&Asn1Time::days_from_now(1).unwrap())
        .unwrap();
    cert.append_extension(BasicConstraints::new().critical().ca().build().unwrap())
        .unwrap();
    let san = SubjectAlternativeName::new()
        .ip("127.0.0.1")
        .build(&cert.x509v3_context(None, None))
        .unwrap();
    cert.append_extension(san).unwrap();
    cert.sign(&key, MessageDigest::sha256()).unwrap();
    let cert = cert.build();
    fs::write(certificate, cert.to_pem().unwrap()).unwrap();

    let mut tls = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).unwrap();
    tls.set_certificate(&cert).unwrap();
    tls.set_private_key(&key).unwrap();
    tls.check_private_key().unwrap();
    tls.build()
}

fn read_line(stream: &mut impl BufRead) -> String {
    let mut line = String::new();
    assert_ne!(
        stream.read_line(&mut line).unwrap(),
        0,
        "unexpected relay EOF"
    );
    line
}

pub(super) async fn accept_mail(tls: SslAcceptor, stream: TcpStream) -> String {
    let stream = stream.into_std().unwrap();
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    tokio::task::spawn_blocking(move || {
        let mut smtp = BufReader::new(tls.accept(stream).unwrap());
        smtp.get_mut()
            .write_all(b"220 relay.example.invalid ESMTP\r\n")
            .unwrap();
        assert!(read_line(&mut smtp).starts_with("EHLO "));
        smtp.get_mut()
            .write_all(b"250 relay.example.invalid\r\n")
            .unwrap();
        assert_eq!(
            read_line(&mut smtp).trim_end(),
            format!("MAIL FROM:<{SENDER}>")
        );
        smtp.get_mut()
            .write_all(b"250 Sender accepted\r\n")
            .unwrap();
        assert_eq!(
            read_line(&mut smtp).trim_end(),
            format!("RCPT TO:<{RECIPIENT}>")
        );
        smtp.get_mut()
            .write_all(b"250 Recipient accepted\r\n")
            .unwrap();
        assert_eq!(read_line(&mut smtp), "DATA\r\n");
        smtp.get_mut()
            .write_all(b"354 End with a single dot\r\n")
            .unwrap();

        let mut message = String::new();
        loop {
            let line = read_line(&mut smtp);
            if line == ".\r\n" {
                break;
            }
            message.push_str(&line);
        }
        smtp.get_mut()
            .write_all(b"250 Message accepted\r\n")
            .unwrap();
        // Gracefully close the one-message SMTP session when curl sends QUIT.
        let mut quit = String::new();
        if smtp.read_line(&mut quit).unwrap() != 0 {
            assert_eq!(quit, "QUIT\r\n");
            smtp.get_mut()
                .write_all(b"221 Closing connection\r\n")
                .unwrap();
        }
        let _ = smtp.get_mut().shutdown();
        message
    })
    .await
    .unwrap()
}
