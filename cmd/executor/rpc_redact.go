package main

import (
	"errors"
	"net/url"
	"strings"
)

// redactRPCURL returns a safe-to-log form of an RPC URL by keeping only the
// scheme + host and replacing any path / query / fragment with a single
// [REDACTED] marker. Provider URL formats vary — Alchemy puts the key as
// the last path segment, Infura as well, QuickNode puts it as an
// intermediate segment between slashes — so masking anything past the host
// is the only shape that doesn't leak secrets on at least one provider.
//
// Falls back to a bare [REDACTED] when the URL is unparseable or has no
// host. The result is assembled manually rather than via url.URL.String()
// because the latter percent-encodes "[" / "]" (produces %5BREDACTED%5D),
// which is unreadable in logs.
func redactRPCURL(raw string) string {
	if raw == "" {
		return ""
	}
	u, err := url.Parse(raw)
	if err != nil || u.Host == "" {
		return "[REDACTED]"
	}
	var b strings.Builder
	if u.Scheme != "" {
		b.WriteString(u.Scheme)
		b.WriteString("://")
	}
	b.WriteString(u.Host)
	if u.Path != "" || u.RawQuery != "" || u.Fragment != "" {
		b.WriteString("/[REDACTED]")
	}
	return b.String()
}

// redactRPCError returns a new error whose message has every occurrence of
// rpcURL replaced with its redacted form. Go-ethereum wraps HTTP failures as
// `Post "<url>": <cause>`, so this keeps the cause visible while stripping
// the embedded API key from logs, journald, Loki, and alert payloads.
//
// Returns nil if err is nil, and the original error unchanged if rpcURL is
// empty.
func redactRPCError(err error, rpcURL string) error {
	if err == nil {
		return nil
	}
	if rpcURL == "" {
		return err
	}
	msg := err.Error()
	redacted := redactRPCURL(rpcURL)
	stripped := strings.ReplaceAll(msg, rpcURL, redacted)
	if stripped == msg {
		return err
	}
	return errors.New(stripped)
}
