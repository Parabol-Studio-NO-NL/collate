# Collate security boundaries

- Share invitations privately. They are bearer credentials. Restarting the host revokes the old invitation. Per-participant access revocation and host migration are pending.
- iroh authenticates endpoints and encrypts transport. The optional relay forwards encrypted packets. The host can read and validate every application payload because the host sequences and stores the document edits.
- Network input is size bounded. Remote actors can submit typed proposals. They cannot name commands, touch file paths or use the local control endpoint. The adapter rejects unsupported edits atomically. Seed archives are bounded before and after expansion. Imported artwork is checked under decoder limits, including PDF object streams and embedded images. Keep the DesignCraft control server on its existing loopback boundary.
- The macOS journal uses an OS Keychain wrapping key and authenticated XChaCha20-Poly1305 ciphertext. It checks file permissions, holds an exclusive record lease and replaces records atomically. Keys and invitation secrets stay out of the native document and app logs. Native files explicitly saved by the owner remain ordinary DesignCraft archives.
- Review upstream workflow and tooling changes before pushing an integration branch. Public CI jobs use read-only tokens and run without release credentials.
- Parabol keeps signing credentials and the official macOS release pipeline private.
- Report collaboration or updater vulnerabilities privately to the maintainers.
