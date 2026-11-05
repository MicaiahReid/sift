//! Canonical IDL resolution through the SPL Program Metadata program.

use std::{io::Read, str::FromStr};

use anyhow::{Context, Result, bail};
use borsh::BorshDeserialize;
use flate2::read::{GzDecoder, ZlibDecoder};
use solana_address::Address;
use spl_program_metadata_client::{
    accounts::Metadata as ProgramMetadata,
    types::{AccountDiscriminator, Compression, DataSource, Encoding, ExternalData, Format},
};
use tracing::debug;

use crate::RpcClient;

/// The public Program Metadata deployment.
pub const PROGRAM_METADATA_ID: &str = "ProgM6JCCvbYkfKqJYHePx4xxSUSqJp7rh8Lyv7nk7S";
// The pinned generated client decodes `data_length` as a four-byte Borsh field,
// while the on-chain Header inserts five bytes of alignment padding before data.
const GENERATED_DATA_PADDING_LEN: usize = 5;

/// A canonical metadata location whose header and authority were verified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalIdl {
    /// Canonical Program Metadata PDA.
    pub address: String,
    /// Decoded JSON IDL text.
    pub text: String,
}

/// Derives, validates, and reads a canonical `idl` metadata record.
pub async fn discover_canonical_idl(client: &RpcClient, program: &str) -> Result<CanonicalIdl> {
    let program_key = parse_address(program)?;
    let metadata_program = parse_address(PROGRAM_METADATA_ID)?;
    let mut seed = [0_u8; 16];
    seed[..3].copy_from_slice(b"idl");
    let (address, _) =
        Address::find_program_address(&[program_key.as_ref(), &seed], &metadata_program);
    load_at(client, program, &address.to_string()).await
}

/// Revalidates and reads a previously pinned canonical metadata account.
pub async fn load_canonical_idl(
    client: &RpcClient,
    program: &str,
    address: &str,
) -> Result<CanonicalIdl> {
    let expected = discover_address(program)?;
    if expected != address {
        bail!("on-chain IDL account {address} is not the canonical idl PDA ({expected})")
    }
    load_at(client, program, address).await
}

fn discover_address(program: &str) -> Result<String> {
    let program_key = parse_address(program)?;
    let metadata_program = parse_address(PROGRAM_METADATA_ID)?;
    let mut seed = [0_u8; 16];
    seed[..3].copy_from_slice(b"idl");
    Ok(
        Address::find_program_address(&[program_key.as_ref(), &seed], &metadata_program)
            .0
            .to_string(),
    )
}

async fn load_at(client: &RpcClient, program: &str, address: &str) -> Result<CanonicalIdl> {
    debug!(
        program,
        idl_address = address,
        "loading canonical on-chain IDL account"
    );
    let account = client
        .account(address)
        .await?
        .ok_or_else(|| anyhow::anyhow!("canonical on-chain IDL account does not exist"))?;
    if account.owner != PROGRAM_METADATA_ID {
        bail!("canonical on-chain IDL account is not owned by Program Metadata")
    }
    let metadata = ProgramMetadata::from_bytes(&account.data)
        .context("cannot decode Program Metadata account")?;
    debug!(?metadata, "loaded canonical on-chain IDL metadata");
    if metadata.discriminator != AccountDiscriminator::Metadata
        || metadata.program != parse_address(program)?
        || metadata.seed != *b"idl\0\0\0\0\0\0\0\0\0\0\0\0\0"
    {
        bail!("on-chain metadata does not describe this program's idl")
    }
    if !metadata.canonical {
        bail!("on-chain metadata is not canonical")
    }
    let text = metadata_content(&metadata, client).await?;
    Ok(CanonicalIdl {
        address: address.to_owned(),
        text,
    })
}

async fn metadata_content(metadata: &ProgramMetadata, client: &RpcClient) -> Result<String> {
    let payload = metadata_payload(metadata)?;
    match metadata.data_source {
        DataSource::Direct => decode_content(
            payload,
            metadata.compression,
            metadata.encoding,
            metadata.format,
        ),
        DataSource::Url => {
            let url = decode_content(
                payload,
                metadata.compression,
                metadata.encoding,
                metadata.format,
            )?;
            let url = url::Url::parse(&url).context("metadata URL is invalid")?;
            if !matches!(url.scheme(), "http" | "https") {
                bail!("metadata URL must use HTTP(S)")
            }
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::limited(3))
                .build()?
                .get(url)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await
                .map_err(Into::into)
        }
        DataSource::External => {
            let external = ExternalData::try_from_slice(payload)
                .context("external metadata reference is invalid")?;
            let offset = external.offset as usize;
            let length = external
                .length
                .to_string()
                .parse::<usize>()
                .context("external metadata length is invalid")?;
            let account = client
                .account(&external.address.to_string())
                .await?
                .ok_or_else(|| anyhow::anyhow!("external metadata account does not exist"))?;
            let end = if length == 0 {
                account.data.len()
            } else {
                offset
                    .checked_add(length)
                    .ok_or_else(|| anyhow::anyhow!("external metadata range overflows"))?
            };
            let data = account
                .data
                .get(offset..end)
                .ok_or_else(|| anyhow::anyhow!("external metadata range is outside its account"))?;
            decode_content(
                data,
                metadata.compression,
                metadata.encoding,
                metadata.format,
            )
        }
    }
}

fn metadata_payload(metadata: &ProgramMetadata) -> Result<&[u8]> {
    let end = GENERATED_DATA_PADDING_LEN
        .checked_add(metadata.data_length as usize)
        .ok_or_else(|| anyhow::anyhow!("metadata data length overflows"))?;
    metadata
        .data
        .get(GENERATED_DATA_PADDING_LEN..end)
        .ok_or_else(|| anyhow::anyhow!("metadata content is shorter than its declared data length"))
}

fn decode_content(
    data: &[u8],
    compression: Compression,
    encoding: Encoding,
    format: Format,
) -> Result<String> {
    let mut expanded = Vec::new();
    match compression {
        Compression::None => expanded.extend_from_slice(data),
        Compression::Gzip => {
            GzDecoder::new(data).read_to_end(&mut expanded)?;
        }
        Compression::Zlib => {
            ZlibDecoder::new(data).read_to_end(&mut expanded)?;
        }
    }
    let raw = match encoding {
        Encoding::None => Ok(hex::encode(expanded)),
        Encoding::Utf8 => String::from_utf8(expanded).context("metadata UTF-8 is invalid"),
        Encoding::Base58 => {
            let encoded = String::from_utf8(expanded).context("metadata base58 is not UTF-8")?;
            String::from_utf8(bs58::decode(encoded).into_vec()?)
                .context("decoded metadata base58 is not UTF-8")
        }
        Encoding::Base64 => {
            use base64::Engine;
            let encoded = String::from_utf8(expanded).context("metadata base64 is not UTF-8")?;
            String::from_utf8(base64::engine::general_purpose::STANDARD.decode(encoded)?)
                .context("decoded metadata base64 is not UTF-8")
        }
    }?;

    match format {
        Format::Json => Ok(serde_json::to_string(&serde_json::Value::from_str(&raw)?)?),
        Format::Yaml => bail!("on-chain IDL format is YAML, which is not supported"),
        Format::Toml => bail!("on-chain IDL format is TOML, which is not supported"),
        Format::None => Ok(raw),
    }
}

fn parse_address(value: &str) -> Result<Address> {
    value
        .parse()
        .map_err(|error| anyhow::anyhow!("invalid address {value}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{ProgramMetadata, metadata_payload};

    #[test]
    fn skips_the_program_header_alignment_padding() {
        let mut bytes = vec![0_u8; 99];
        bytes[87..91].copy_from_slice(&3_u32.to_le_bytes());
        bytes[96..99].copy_from_slice(&[1, 2, 3]);
        let metadata = ProgramMetadata::from_bytes(&bytes).unwrap();
        assert_eq!(metadata_payload(&metadata).unwrap(), [1, 2, 3]);
    }
}
