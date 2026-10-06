#!/usr/bin/env bash
# Regenerate rsa_issuer_ecdsa_subject.pem: an RSA certification authority
# and an ECDSA (P-256) certificate it signs, issuer first. The shape whose
# signature must be checked with the issuer's key, not the subject's.
set -euo pipefail
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
openssl genrsa -out "$work/ca.key" 2048 2>/dev/null
openssl req -x509 -new -key "$work/ca.key" -sha256 -days 3650 \
    -subj "/CN=Morpholog test RSA CA" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,keyCertSign" \
    -out "$work/ca.pem"
openssl ecparam -name prime256v1 -genkey -noout -out "$work/leaf.key"
openssl req -new -key "$work/leaf.key" -subj "/CN=Morpholog test ECDSA subject" -out "$work/leaf.csr"
openssl x509 -req -in "$work/leaf.csr" -CA "$work/ca.pem" -CAkey "$work/ca.key" \
    -CAcreateserial -sha256 -days 3650 -out "$work/leaf.pem"
cat "$work/ca.pem" "$work/leaf.pem" > rsa_issuer_ecdsa_subject.pem
