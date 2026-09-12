use super::NetworkSeccompMode;
use super::network_seccomp_filter_file;
use pretty_assertions::assert_eq;
use std::io::Read;
use std::io::Seek;
use std::io::Write;

#[test]
fn exported_network_filters_are_readable_from_start_and_immutable() {
    for mode in [
        NetworkSeccompMode::Restricted,
        NetworkSeccompMode::ProxyRouted,
    ] {
        let mut file = network_seccomp_filter_file(mode).expect("export network filter");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .expect("read filter from initial offset");
        assert!(
            !bytes.is_empty(),
            "Bubblewrap must receive the complete filter"
        );
        assert_eq!(
            bytes.len() as u64,
            file.metadata().expect("filter metadata").len()
        );
        file.rewind().expect("rewind for in-place write attempt");
        assert_eq!(
            file.write_all(&[0])
                .expect_err("filter must reject writes")
                .raw_os_error(),
            Some(libc::EPERM)
        );
        assert_eq!(
            file.set_len(bytes.len() as u64 + 1)
                .expect_err("filter must reject growth")
                .raw_os_error(),
            Some(libc::EPERM)
        );
        assert_eq!(
            file.set_len(0)
                .expect_err("filter must reject truncation")
                .raw_os_error(),
            Some(libc::EPERM)
        );
    }
}
