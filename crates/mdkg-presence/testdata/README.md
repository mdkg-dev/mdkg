Test-only attestation certificates, made with openssl for the unit tests
(`src/testutil.rs`). None of it protects anything; every file is
throwaway.

- `root.der`: a P-256 CA root. `other-root.der`: an unrelated one.
- `leaf.der`: an attestation leaf the root signed, fitting the packed
  profile (v3, OU "Authenticator Attestation", CA false).
- `leaf-aaguid.der`: the same, with an id-fido-gen-ce-aaguid extension of
  `[0xaa; 16]` (the fake authenticator's AAGUID); `leaf-aaguid-other.der`
  names `[0xbb; 16]`.
- `leaf-ou.der` (OU "Other Unit") and `leaf-ca.der` (CA true): leaves
  that fail the profile, signed by the root.
- `inter-certsign.der` (CA, KeyUsage keyCertSign and cRLSign) and
  `inter-crlonly.der` (CA, KeyUsage cRLSign only): intermediates the
  root signed; `inter-certsign-leaf.der` and `inter-crlonly-leaf.der`
  are profile-fitting leaves each signed.
- `leaf-sha512.der`: a profile-fitting leaf the root signed with ECDSA
  and SHA-512, an algorithm the chain check doesn't verify.
- `leaf-key.pk8`: the PKCS#8 key of every leaf above.
- `leaf-rsa.der` and `leaf-rsa-key.pk8`: a self-signed RSA 2048
  attestation leaf and its key, for RS256.

All are valid from 2026-10-29 for 100 years.
