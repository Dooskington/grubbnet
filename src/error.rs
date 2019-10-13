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

    #[doc(hidden)]
    __Nonexhaustive,
}
