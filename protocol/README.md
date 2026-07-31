# Protocol

This directory will contain the versioned protocol schemas shared by the CLI and mobile SDKs.

The first protocol slice should define:

- handshake and capability negotiation;
- request, success, and error envelopes;
- snapshot generations and stable element references;
- compact tree queries and explicit truncation metadata;
- screenshot artifact metadata;
- action requests and before/after evidence.
