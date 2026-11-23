#!/bin/zsh
# One-time: create a local, self-signed code-signing identity "Juan Terminal Dev" in your
# login keychain. build-app.sh signs with it when present, so every rebuild keeps the same
# identity and macOS stops re-asking for folder permissions (Desktop, Documents…) after each
# install. Nothing leaves this Mac.
set -euo pipefail
NAME="Juan Terminal Dev"
if security find-identity -p codesigning 2>/dev/null | grep -q "$NAME"; then
  echo "\"$NAME\" already exists."; exit 0
fi
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
cat > "$TMP/cfg" <<CFG
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $NAME
[ext]
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
basicConstraints = critical, CA:false
CFG
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -config "$TMP/cfg" -keyout "$TMP/key.pem" -out "$TMP/cert.pem" 2>/dev/null
openssl pkcs12 -export -legacy -inkey "$TMP/key.pem" -in "$TMP/cert.pem" -name "$NAME" -passout pass:jt -out "$TMP/id.p12" 2>/dev/null \
  || openssl pkcs12 -export -inkey "$TMP/key.pem" -in "$TMP/cert.pem" -name "$NAME" -passout pass:jt -out "$TMP/id.p12"
# -T lets codesign use the key without a keychain prompt on every build.
security import "$TMP/id.p12" -k "$HOME/Library/Keychains/login.keychain-db" -P jt -T /usr/bin/codesign
echo "Created \"$NAME\". Rebuild with ./packaging/build-app.sh"
