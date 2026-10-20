module github.com/moq-dev/moq-go-ffi

go 1.21

// Builds use Go 1.27.3+ for the crypto/x509 eMudhra intermediate CA block
// (CVE-2026-111943, go.dev/issue/82345). Older local toolchains auto-switch
// to this one under the default GOTOOLCHAIN=auto.
toolchain go1.27.3
