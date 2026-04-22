package main

import (
	"errors"
	"fmt"
	"strings"
	"testing"
)

func TestRedactRPCURL(t *testing.T) {
	t.Parallel()
	cases := []struct {
		name, in, want string
	}{
		{"empty", "", ""},
		{
			"alchemy",
			"https://eth-mainnet.g.alchemy.com/v2/abc123XYZsecret",
			"https://eth-mainnet.g.alchemy.com/[REDACTED]",
		},
		{
			"quicknode — key is intermediate path segment",
			"https://example.quiknode.pro/abc123XYZsecret/",
			"https://example.quiknode.pro/[REDACTED]",
		},
		{
			"infura",
			"https://mainnet.infura.io/v3/projectidsecret",
			"https://mainnet.infura.io/[REDACTED]",
		},
		{
			"no path",
			"https://host.example",
			"https://host.example",
		},
		{
			"localhost with trailing slash",
			"http://localhost:8545/",
			"http://localhost:8545/[REDACTED]",
		},
		{
			"query string is masked",
			"https://host.example/path?apikey=secret",
			"https://host.example/[REDACTED]",
		},
	}
	for _, tc := range cases {
		tc := tc
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			got := redactRPCURL(tc.in)
			if got != tc.want {
				t.Errorf("redactRPCURL(%q) = %q, want %q", tc.in, got, tc.want)
			}
			if tc.in != "" && strings.Contains(got, "secret") {
				t.Errorf("secret leaked into %q", got)
			}
		})
	}
}

func TestRedactRPCError(t *testing.T) {
	t.Parallel()

	rpcURL := "https://eth-mainnet.g.alchemy.com/v2/ALCHEMYKEY123"

	t.Run("nil err passes through", func(t *testing.T) {
		t.Parallel()
		if redactRPCError(nil, rpcURL) != nil {
			t.Fatal("expected nil, got non-nil")
		}
	})

	t.Run("empty url returns original", func(t *testing.T) {
		t.Parallel()
		orig := errors.New("boom")
		if got := redactRPCError(orig, ""); got != orig {
			t.Errorf("expected identity, got %v", got)
		}
	})

	t.Run("go-ethereum wrapped Post error is redacted", func(t *testing.T) {
		t.Parallel()
		wrapped := fmt.Errorf("Post %q: connection refused", rpcURL)
		got := redactRPCError(wrapped, rpcURL)
		if got == nil {
			t.Fatal("expected non-nil")
		}
		if strings.Contains(got.Error(), "ALCHEMYKEY123") {
			t.Fatalf("api key leaked: %q", got.Error())
		}
		if !strings.Contains(got.Error(), "[REDACTED]") {
			t.Errorf("redaction marker missing: %q", got.Error())
		}
		if !strings.Contains(got.Error(), "connection refused") {
			t.Errorf("underlying cause dropped: %q", got.Error())
		}
	})

	t.Run("error without url is passed through", func(t *testing.T) {
		t.Parallel()
		orig := errors.New("context deadline exceeded")
		got := redactRPCError(orig, rpcURL)
		if got.Error() != orig.Error() {
			t.Errorf("expected %q, got %q", orig.Error(), got.Error())
		}
	})
}
