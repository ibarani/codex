use super::parse_effective_uid;

#[test]
fn effective_owner_is_distinct_from_real_saved_and_filesystem_owners() -> anyhow::Result<()> {
    assert_eq!(
        parse_effective_uid(b"Name:\t\xff\nUid:\t1001\t1002\t1003\t1004\n")?,
        1002
    );
    assert_eq!(parse_effective_uid(b"Uid:\t0\t0\t0\t0\n")?, 0);
    Ok(())
}

#[test]
fn malformed_owner_fields_fail_without_echoing_process_contents() {
    for status in [
        b"Name: PRIVATE-STATUS\n".as_slice(),
        b"Uid: 1 2 3",
        b"Uid: 1 2 3 4 5",
        b"Uid: 1 PRIVATE-STATUS 3 4",
    ] {
        let failure = parse_effective_uid(status);
        assert!(failure.is_err());
        assert!(!format!("{failure:?}").contains("PRIVATE-STATUS"));
    }
}
