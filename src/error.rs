use derive_more::{Display, From};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(From, Display, Debug)]
pub enum Error {
    Io(std::io::Error),

    #[cfg(feature = "crypto")]
    OpenSsl(openssl::error::ErrorStack),

    #[cfg(feature = "crypto")]
    Bcrypt(bcrypt::BcryptError),

    FailedToSendBytes,
    FailedToRegisterForEvents,
    InvalidData,
    ConnectionNotFound,

    /// A packet body was too large to describe in a header. Always a bug in the sending
    /// application rather than something a peer did.
    #[display("packet body of {_0} bytes reaches or exceeds the {_1} byte limit")]
    PacketTooLarge(usize, usize),

    #[doc(hidden)]
    __Nonexhaustive,
}
