1. Identify the target program ID and the program-metadata program ID. For the current SPL program-metadata deployment, the metadata program is:

   ProgM6JCCvbYkfKqJYHePx4xxSUSqJp7rh8Lyv7nk7S

2. Fetch the target program account with getAccountInfo.
   - Confirm it is owned by BPFLoaderUpgradeab1e11111111111111111111.
   - Parse its 36-byte upgradeable-loader Program state to obtain the ProgramData address.
   - Fetch ProgramData and parse its upgrade authority (if any).

3. Decide which PDA forms are possible for each candidate metadata label. Labels are fixed-size 16-byte UTF-8 strings, zero-padded on the right. The normal IDL label is:

   "idl" => 69646c0000000000000000000000000

   Derive these under the program-metadata program ID:

   canonical:
   [ target_program_pubkey_bytes, padded_16_byte_seed ]

   non-canonical:
   [ target_program_pubkey_bytes, metadata_authority_pubkey_bytes, padded_16_byte_seed ]

   The canonical PDA is appropriate only when the metadata writer is the target program’s current upgrade authority. The upgrade authority is not a canonical PDA seed.

4. Check each derived address with getAccountInfo.

   A valid account should:
   - exist;
   - be owned by the metadata program;
   - decode as a Metadata account;
   - store the requested target program ID;
   - have seed idl (after stripping trailing zeroes).

5. If the logical seed is unknown, enumerate instead of guessing:
   - Call getProgramAccounts(metadata_program_id).
   - Filter by the target program ID in the metadata header. In the current account format, the target program begins at byte offset 1 (after the one-byte discriminator).
   - Decode the returned accounts and inspect their stored 16-byte seed field.
   - Select the account whose decoded seed is idl; record whether its canonical flag is true and its stored authority.

6. If more than one idl metadata account exists:
   - Prefer the canonical account.
   - If only non-canonical accounts exist, the correct PDA is authority-specific; retain the metadata authority as part of the lookup key.
   - Do not assume an arbitrary third-party IDL is authoritative.

For the example in the prior task, this resolves to:

PDA program: ProgM6JCCvbYkfKqJYHePx4xxSUSqJp7rh8Lyv7nk7S
seeds: 1. govYkyQ3ePtGULAtY6V75qjWE8UH4vCUVQ1W4HdCAZU 2. "idl" padded to 16 bytes

result:
836SKoXmfvCEaKeiPRiyZhMPaXvepDnhUDEumvWaZCHb

The key practical point: an IDL seed alone is not enough to locate non-canonical metadata; those require the metadata authority as an additional PDA seed.
