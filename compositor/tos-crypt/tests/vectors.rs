//! The published test vectors, run against both halves of this crate.
//!
//! Hand-written cryptography is only defensible when someone else has
//! already said what the answers are, so this file is the reason the rest
//! of the crate is allowed to exist. Nothing here is a value this project
//! chose; every expected digest is copied out of one of two documents.
//!
//! - SHA-512: FIPS 180-4, whose worked examples NIST publishes alongside
//!   the standard, and the byte-oriented vectors from the Cryptographic
//!   Algorithm Validation Program (`SHA512ShortMsg.rsp`). The CAVS
//!   messages below were picked by length rather than at random: 55, 56,
//!   57, 63, 64, 65, 111, 112, 113, 119, 120, 127 and 128 bytes are the
//!   lengths either side of every decision the padding makes, and a
//!   padding bug that survives those is hard to imagine.
//! - `$6$`: Ulrich Drepper's specification at
//!   <https://www.akkadia.org/drepper/SHA-crypt.txt>, all seven of the
//!   vectors in its `tests2[]` array. Between them they cover the default
//!   round count and four explicit ones, a salt longer than the sixteen
//!   characters the scheme keeps, a password longer than one block, and a
//!   `rounds=` too small to be allowed — which the scheme does not reject
//!   but raises to the minimum, and then prints the number it used.
//!
//! Each `$6$` vector is checked twice: that hashing reproduces the line,
//! and that verifying against the published line accepts the password and
//! rejects a near miss. The second is the direction the screen lock runs
//! in, and it is the one that would matter if it were wrong.

use tos_crypt::sha512::sha512;
use tos_crypt::sha512crypt::{hash, hash_with_rounds, verify};

/// FIPS 180-4's two-block example: 112 bytes, so the padding needs a
/// block of its own.
const TWO_BLOCK: &[u8] = b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu";

fn from_hex(hex: &str) -> Vec<u8> {
    assert!(hex.len() % 2 == 0);
    hex.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let digits = std::str::from_utf8(pair).expect("hex is ASCII");
            u8::from_str_radix(digits, 16).expect("hex digit")
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The three worked examples from FIPS 180-4, including the one-million
/// character message, which is the only test here that exercises a message
/// long enough for the length counter to carry past a byte.
#[test]
fn fips_180_4_examples() {
    assert_eq!(hex(&sha512(b"abc")), "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f");
    assert_eq!(hex(&sha512(TWO_BLOCK)), "8e959b75dae313da8cf4f72814fc143f8f7779c6eb9f7fa17299aeadb6889018501d289e4900f7e4331b99dec4b5433ac7d329eeb6dd26545e96e55b874be909");
    let million_a = vec![b'a'; 1_000_000];
    assert_eq!(hex(&sha512(&million_a)), "e718483d0ce769644e2e42c7bc15b4638e1f98b13b2044285632a803afa973ebde0ff244877ea60a4cb0432ce577c31beb009c5c2c49aa2e4eadb217ad8cc09b");
}

/// NIST CAVS `SHA512ShortMsg.rsp`, at the message lengths where the
/// padding either does or does not need a second block.
#[test]
fn nist_cavs_short_messages() {
    // (message length in bytes, message, digest)
    const VECTORS: &[(usize, &str, &str)] = &[
        (0, "", "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"),
        (1, "21", "3831a6a6155e509dee59a7f451eb35324d8f8f2df6e3708894740f98fdee23889f4de5adb0c5010dfb555cda77c8ab5dc902094c52de3278f35a75ebc25f093a"),
        (55, "ec2a92e47f692b53c1355475c71ceff0b0952a8b3541b2938270247d44e7c5cc04e17236b353da028674eab4047d89ec5dad868cfd91ce", "c83aca6147bfcbbc72c377efa8d53654ba0830c5a6a89e1d2a19b713e68fb534640deb833ca512247166dd273b5897e57d526f88eef58f6ff97baee0b4ee5644"),
        (56, "c99e31ad4e23ac68e15e605d0b02437f8147c44f5445a55b68a10905276cce8676481c33e8cd3efe322bb13fe0107bb546ccbec7b8b38d10", "52992d45a88221d972958e9f2854adaa9a21d2bf7051e1f1019ae78004da50c5b55c144a02afffe539d753949a2b056534f5b4c21f248a05baa52a6c38c7f5dd"),
        (57, "9aa3e8ad92777dfeb121a646ce2e918d1e12b30754bc09470d6da4af6cc9642b012f041ff046569d4fd8d0dccfe448e59feefc908d9ad5af6f", "994d1cda4de40aff4713237cf9f78f7033af83369ac9c64e504091ea2f1caff6c5152d6a0c5608f82886c0093b3d7fbadd49dfd1f9e0f85accf23bc7dad48904"),
        (63, "ebb3e2ad7803508ba46e81e220b1cff33ea8381504110e9f8092ef085afef84db0d436931d085d0e1b06bd218cf571c79338da31a83b4cb1ec6c06d6b98768", "f33428d8fc67aa2cc1adcb2822f37f29cbd72abff68190483e415824f0bcecd447cb4f05a9c47031b9c50e0411c552f31cd04c30cea2bc64bcf825a5f8a66028"),
        (64, "c1ca70ae1279ba0b918157558b4920d6b7fba8a06be515170f202fafd36fb7f79d69fad745dba6150568db1e2b728504113eeac34f527fc82f2200b462ecbf5d", "046e46623912b3932b8d662ab42583423843206301b58bf20ab6d76fd47f1cbbcf421df536ecd7e56db5354e7e0f98822d2129c197f6f0f222b8ec5231f3967d"),
        (65, "d3ddddf805b1678a02e39200f6440047acbb062e4a2f046a3ca7f1dd6eb03a18be00cd1eb158706a64af5834c68cf7f105b415194605222c99a2cbf72c50cb14bf", "bae7c5d590bf25a493d8f48b8b4638ccb10541c67996e47287b984322009d27d1348f3ef2999f5ee0d38e112cd5a807a57830cdc318a1181e6c4653cdb8cf122"),
        (111, "324533e685f1852e358eea8ea8b81c288b3f3beb1f2bc2b8d3fdbac318382e3d7120de30c9c237aa0a34831deb1e5e060a7969cd3a9742ec1e64b354f7eb290cba1c681c66cc7ea994fdf5614f604d1a2718aab581c1c94931b1387e4b7dc73635bf3a7301174075fa70a9227d85d3", "3b26c5170729d0814153becb95f1b65cd42f9a6d0649d914e4f69d938b5e9dc041cd0f5c8da0b484d7c7bc7b1bdefb08fe8b1bfedc81109345bc9e9a399feedf"),
        (112, "518985977ee21d2bf622a20567124fcbf11c72df805365835ab3c041f4a9cd8a0ad63c9dee1018aa21a9fa3720f47dc48006f1aa3dba544950f87e627f369bc2793ede21223274492cceb77be7eea50e5a509059929a16d33a9f54796cde5770c74bd3ecc25318503f1a41976407aff2", "c00926a374cde55b8fbd77f50da1363da19744d3f464e07ce31794c5a61b6f9c85689fa1cfe136553527fd876be91673c2cac2dd157b2defea360851b6d92cf4"),
        (113, "9159767275ba6f79cbb3d58c0108339d8c6a41138991ab7aa58b14793b545b04bda61dd255127b12cc501d5aaad476e09fa14aec21626e8d57b7d08c36cdb79eea314bdd77e65779a0b54eab08c48ceb976adf631f4246a33f7ef896887ea8b5dfa2087a225c8c180f8970696101fc283b", "3cd3380a90868de17dee4bd4d7f90d7512696f0a92b2d089240d61a9d20cd3af094c78bf466c2d404dd2f662ec5f4a299be2adeadf627b98e50e1c072b769d62"),
        (119, "36af17595494ef793c42f48410246df07d05936a918afe74cd005e537c586b2843701f5df8952242b74586f83339b48f4ba3a66bdeb457ecdf61784eac6765cd9b8c570dd628dbba6ae5836b9ac3dbcd795f9efdb8742a35bca232abf36eb3b6698b2933965802277ba953a6edcacaf330c1e4e8c7d45f", "158bfc348a30b4fabbe355a7d44bdc2122a4c850444c03f289003ce01bfc1ebf3ecc0febb6a8ff523d25db7681b05bdce048d11943ab476c1967cf6556c4a120"),
        (120, "42d66edc5f22e0c13c25504c5101a5d172d2db7209e461efa323c0bfaed27e5f808042ea9c3838ea31f9b76de465225ccfbd0c09ca0d9f07e9a43e3e46c7693e00a7e1d483900ddb0a629d5563456dbbf299ac91f92c3d3c17b05d180e6c87c6c93194c39d90273fcf4a482c56084f95e34c04311fa80438", "061afb119a3c60876e04c10f12ad0f4b977593dc5a2d21096a57e7d3f7d4d44fdef934b2c17d7530674e4f4a1c176dbdcc54811a22e1b8712e4192fc2d4bf8e8"),
        (127, "c13e6ca3abb893aa5f82c4a8ef754460628af6b75af02168f45b72f8f09e45ed127c203bc7bb80ff0c7bd96f8cc6d8110868eb2cfc01037d8058992a6cf2effcbfe498c842e53a2e68a793867968ba18efc4a78b21cdf6a11e5de821dcabab14921ddb33625d48a13baffad6fe8272dbdf4433bd0f7b813c981269c388f001", "6e56f77f6883d0bd4face8b8d557f144661989f66d51b1fe4b8fc7124d66d9d20218616fea1bcf86c08d63bf8f2f21845a3e519083b937e70aa7c358310b5a7c"),
        (128, "fd2203e467574e834ab07c9097ae164532f24be1eb5d88f1af7748ceff0d2c67a21f4e4097f9d3bb4e9fbf97186e0db6db0100230a52b453d421f8ab9c9a6043aa3295ea20d2f06a2f37470d8a99075f1b8a8336f6228cf08b5942fc1fb4299c7d2480e8e82bce175540bdfad7752bc95b577f229515394f3ae5cec870a4b2f8", "a21b1077d52b27ac545af63b32746c6e3c51cb0cb9f281eb9f3580a6d4996d5c9917d2a6e484627a9d5a06fa1b25327a9d710e027387fc3e07d7c4d14c6086cc"),
    ];
    for (len, message, digest) in VECTORS {
        let message = from_hex(message);
        assert_eq!(message.len(), *len);
        assert_eq!(hex(&sha512(&message)), *digest, "{len}-byte message");
    }
}

/// The `$6$` vectors, hashed. Salt and rounds are given the way the
/// specification gives them, including the two that are out of range and
/// come back changed.
#[test]
fn drepper_vectors_hash() {
    for (salt, rounds, password, expected) in VECTORS {
        let line = match rounds {
            Some(rounds) => hash_with_rounds(password.as_bytes(), salt.as_bytes(), *rounds),
            None => hash(password.as_bytes(), salt.as_bytes()),
        };
        assert_eq!(line, *expected, "salt {salt:?}");
    }
}

/// The same vectors, verified — which is what the lock does. A password
/// one character short of right has to be rejected as a password rather
/// than reported as a broken line.
#[test]
fn drepper_vectors_verify() {
    for (_, _, password, expected) in VECTORS {
        assert_eq!(
            verify(password.as_bytes(), expected),
            Ok(true),
            "{expected}"
        );
        let near_miss = &password[..password.len() - 1];
        assert_eq!(
            verify(near_miss.as_bytes(), expected),
            Ok(false),
            "{expected}"
        );
    }
}

/// The seven vectors of `tests2[]`, split into the salt, the round count
/// the salt string asked for, the password and the line that comes out.
const VECTORS: &[(&str, Option<u32>, &str, &str)] = &[
    (
        "saltstring",
        None,
        "Hello world!",
        "$6$saltstring$svn8UoSVapNtMuq1ukKS4tPQd8iKwSMHWjl/O817G3uBnIFNjnQJuesI68u4OTLiBFdcbYEdFCoEOfaS35inz1",
    ),
    (
        "saltstringsaltstring",
        Some(10000),
        "Hello world!",
        "$6$rounds=10000$saltstringsaltst$OW1/O6BYHV6BcXZu8QVeXbDWra3Oeqh0sbHbbMCVNSnCM/UrjmM0Dp8vOuZeHBy/YTBmSK6H9qs/y3RnOaw5v.",
    ),
    (
        "toolongsaltstring",
        Some(5000),
        "This is just a test",
        "$6$rounds=5000$toolongsaltstrin$lQ8jolhgVRVhY4b5pZKaysCLi0QBxGoNeKQzQ3glMhwllF7oGDZxUhx1yxdYcz/e1JSbq3y6JMxxl8audkUEm0",
    ),
    (
        "anotherlongsaltstring",
        Some(1400),
        "a very much longer text to encrypt.  This one even stretches over morethan one line.",
        "$6$rounds=1400$anotherlongsalts$POfYwTEok97VWcjxIiSOjiykti.o/pQs.wPvMxQ6Fm7I6IoYN3CmLs66x9t0oSwbtEW7o7UmJEiDwGqd8p4ur1",
    ),
    (
        "short",
        Some(77777),
        "we have a short salt string but not a short password",
        "$6$rounds=77777$short$WuQyW2YR.hBNpjjRhpYD/ifIw05xdfeEyQoMxIXbkvr0gge1a1x3yRULJ5CCaUeOxFmtlcGZelFl5CxtgfiAc0",
    ),
    (
        "asaltof16chars..",
        Some(123456),
        "a short string",
        "$6$rounds=123456$asaltof16chars..$BtCwjqMJGx5hrJhZywWvt0RLE8uZ4oPwcelCjmw2kSYu.Ec6ycULevoBK25fs2xXgMNrCzIMVcgEJAstJeonj1",
    ),
    (
        "roundstoolow",
        Some(10),
        "the minimum number is still observed",
        "$6$rounds=1000$roundstoolow$kUMsbe306n21p9R.FRkW3IGn.S9NPN0x50YhH1xhLsPuWGsUSklZt58jaTfF4ZEQpyUNGc0dqbpBYYBaHHrsX.",
    ),
];
