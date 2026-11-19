//! 可被集成测试与 lib 单元测试共同引用的 SSH key 夹具。
//!
//! 只放静态常量，不引用任何产品 crate，既能在 `tests/` 集成测试中作为 support 模块，
//! 也能通过 `#[path]` 编进 lib 的测试模块；key 仅用于 loopback，不作为产品凭据。
//!
//! @author kongweiguang

/// 本机生成的 2048 位 RSA 测试私钥（无 passphrase），只用于 loopback 认证用例；
/// 与 server fixture key 一样不得作为产品凭据复用。
const TEST_RSA_PRIVATE_KEY: &str = "-----BEGIN RSA PRIVATE KEY-----
MIIEpAIBAAKCAQEA9DvbopTvacL3Hguco8j3zUeXgW4CDUI0MgiW+wrZLwT91BTk
VLrk32shSbLvBMCtGsLgv/324OCIR34ZHE/XatytovaIOKYDSS++DZOvozxR8QAN
dWVt4i2aPjryBORcs4+IJ6Nkj98OExhGqbVeDMfbtJGhbGIhXT+yIMbGCY3dbTzg
vHOhYJx7rRlePHhDkFDBLlP/lCt/YjnmdjCRb0cGWUnwyojASCtQuoawGQpm5+Ut
t4ulbDmPRzWKLh9621x1QtgxGEN0BAe/leFqLB+unQHXBI1j/W9FNtqPXSIV4cvF
SposRXav9IbWJlaqplKFLdhGW7Pw4FdVqSvtVQIDAQABAoIBAAeiGFwDqgXBx8S/
01uW05BhSX7qV+4Sr6CIKP7GieOwty4tKPsPFNJFsUeUH0bxJiKIyGzq6TH5QXPJ
fv7hqYiReVq7lgwxVSZKjUw0IbSee5q72SdNEl8tbBcVSIqWmZz5dZAwHg7iIP0m
MI0N06yJhKuNqz4nIv3yB2u20zcDTPUIpnENYvt04KhvZLWN71Yod29zQNNFCwxs
6wBHpk7EE3UkppRNGsm0SdxwnSmi0SCQcD8/8w4lec37UxrP7Mc0+Ix+a9D7aV7j
WpXsrGpY/OnlI9bqFcxHiAYbO0vQ+2C0Rv8UUJ88nef0J0BN9C+GSrNfykYmaCem
oLDgX9ECgYEA/ElU0M9s3A57ASSDWP9TVfZxsBR8gHSpRWBHJ8HfB3vDfu28OhMa
9VJC5tknLEN3y8OmxuGnz3oSujtVvnJ/CA+fA5Pt90T17pCesmuZoiNUXScBKIdA
7uZwxdGZWgEzjq84vULwnQKxDTNNC15Jah51LxCSQZsk0VuCjtn7CRMCgYEA99Qu
wH12YqDWim8dwn7nuYeQpP3524/1429wKr2WAarCNaUh7dtlpB1RdEm3gNHxP5Bv
GA0ogVySSn+3zG6w/1pNhcOkGKM9KaRYxPK2rKkIAK2BYaBA5zZ1C1bZg2a6mi8v
P5plX1s/T3YC7pYTBfyjxAZuimRrVvRV4jwCpPcCgYEA7FGQlkxGb1dNF0PpmVsJ
5/MCiWqXOQiOasMFxBMRZIQlGud8MnYlAXk929m7nPXR6FfsCE3mEC4MDtB3v/6m
SZnAXOybssyxBfgH/59U+RDY/K0Fo74WL/JB3rs4R5vqfJRbtMVYd+rGiFPurmBy
qmm/YQfHyJaYBVgP0toTEWsCgYEA9Y1oEVN+hDTGvf1g6EBjiyF2QPRgp5rZUFMv
OToFsnsl2I9YsdtLtzNRbL5JsrrRpWCPoq4O7zXwwR4Q197pQumPnDT/l2XOiT8M
sX+nXiZRBpLjef6VsEH18JlKBXc49uI/l258eOHpNetA9nL+QFVVUjnpNvhOnqaT
nghFunsCgYAdYlMfTaeXEOw6+kR9RtXNWps2GLfZTjaVxN1Hb5Kw913skdxf/pf5
ewDBMNBqVCMgTCdVxMpFNCagwMGXdKAbVZO109idzG9roysJKnrDNlXjpXe5/OQw
oX/qwmewXSfQo94wMZ4pl+4BRshn5WmItN5S2f8DM2WZW5H0IrtAmw==
-----END RSA PRIVATE KEY-----";

/// 本机生成的 OpenSSH 格式 Ed25519 测试私钥（无 passphrase）。
/// 选择静态夹具而不是运行时生成，是为了让回归用例可直接对应常见 `id_ed25519` 文件，
/// 且不引入第二套 RNG 依赖；同样只允许用于 loopback 测试。
const TEST_ED25519_PRIVATE_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACC471WlXMfCRDqs3l0jjRRbNlLh6Odt2Hx6Dlp7zk7kzQAAAJhOWrHYTlqx
2AAAAAtzc2gtZWQyNTUxOQAAACC471WlXMfCRDqs3l0jjRRbNlLh6Odt2Hx6Dlp7zk7kzQ
AAAEBan17XwMbQ5I8d4kXt/b1yY/E54HSkq2pLFAg8mwR13rjvVaVcx8JEOqzeXSONFFs2
UuHo523YfHoOWnvOTuTNAAAAFWtlcm1pbmFsLXRlc3QtZWQyNTUxOQ==
-----END OPENSSH PRIVATE KEY-----";

/// 本机生成的 OpenSSH 格式 ECDSA P-256 测试私钥（无 passphrase）。
/// 覆盖 OpenSSH 客户端默认 `id_ecdsa` 的 nistp256 算法；服务端 fixture 只接受公钥
/// 协议流，因此该密钥验证的是客户端加载与签名路径，不是 authorized_keys 策略。
const TEST_ECDSA_P256_PRIVATE_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAaAAAABNlY2RzYS
1zaGEyLW5pc3RwMjU2AAAACG5pc3RwMjU2AAAAQQT3dXCgc9ImZ4MoVbS20JFvRldcwB2c
KZza4OfFFrendv5n3HwLgzutRzUsKhg4RTpnIh5FlcCVW8QOhA6pwiVsAAAAsORmByTkZg
ckAAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBPd1cKBz0iZngyhV
tLbQkW9GV1zAHZwpnNrg58UWt6d2/mfcfAuDO61HNSwqGDhFOmciHkWVwJVbxA6EDqnCJW
wAAAAgcykDNByvc4lj/eB5RFYVSlcwqcHChRZUL/Nk02MxT6MAAAATa2VybWluYWwtdGVz
dC1lY2RzYQECAwQF
-----END OPENSSH PRIVATE KEY-----";

/// 本机生成的 OpenSSH 格式 Ed25519 测试私钥，使用 bcrypt-pbkdf + AES-256-CTR 加密。
/// 带口令私钥必须走与无口令私钥不同的解密分支；固定夹具让“错误口令后重试”可重复，
/// 口令本身也只用于 loopback 测试。
const TEST_PASSPHRASE_PRIVATE_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABBJNtHwBz
eXSKnQdmXkuDRwAAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIJ/KWOfXH4/hzMZS
PvNrLGTSIaLvE9duVVqDIzBXORAtAAAAsD655yhlv2KdqMcfZ+l2vDDr1SmkQuW6v3r6BP
ftFKm0HGQ8bOEcNRSPlqvAeCQUbMy+VA2uOCz5ykC1u3EP8ensLJtcEHGBh/omhYgoKbf8
eoefBSU9mN4OPr/OCNdT0mrvklRw5umcTSMmMRAexf4Ub5xKaaXawT57fXCBP9PL7PEY7Z
c5tS03C0BiwpuQgITF6w2u6TE4Z32lWZ/pMxcFuH7A3RjjD4agAZB1dnb+
-----END OPENSSH PRIVATE KEY-----";

/// 带口令夹具的测试口令；测试通过产品 resolver 的 KeyPassphrase 分支传入，
/// 不写入配置或日志。
pub const TEST_KEY_PASSPHRASE: &str = "kerminal-test-passphrase";

/// 返回 RSA 认证用例专用私钥副本：同一公钥可分别用 ssh-rsa 与 rsa-sha2-* 签名，
/// 便于断言协商出的算法。
/// @author kongweiguang
pub fn test_rsa_private_key() -> Vec<u8> {
    TEST_RSA_PRIVATE_KEY.as_bytes().to_vec()
}

/// 返回 Ed25519 认证用例专用私钥副本，保持与 RSA 夹具一致的调用方式。
/// @author kongweiguang
pub fn test_ed25519_private_key() -> Vec<u8> {
    TEST_ED25519_PRIVATE_KEY.as_bytes().to_vec()
}

/// 返回 ECDSA P-256 认证用例专用私钥副本；曲线固定为 nistp256，
/// 与 OpenSSH 默认 `id_ecdsa` 一致。
/// @author kongweiguang
pub fn test_ecdsa_p256_private_key() -> Vec<u8> {
    TEST_ECDSA_P256_PRIVATE_KEY.as_bytes().to_vec()
}

/// 返回带口令 Ed25519 私钥副本；调用方必须同时提供 `TEST_KEY_PASSPHRASE`
/// 才能完成产品解密分支。
/// @author kongweiguang
pub fn test_passphrase_private_key() -> Vec<u8> {
    TEST_PASSPHRASE_PRIVATE_KEY.as_bytes().to_vec()
}
