# Homebrew formula stub for `browser-control`.
#
# This file is a TEMPLATE. The release workflow (`.github/workflows/release.yml`,
# `homebrew-bump` job) renders it with concrete values via `sed` and commits
# the result to `Formula/browser-control.rb` in this repo (Pattern A:
# in-repo tap). Users install with:
#
#   brew tap rickardp/browser-control https://github.com/rickardp/browser-control.git
#   brew install browser-control
#
# Placeholders rendered by the workflow (each surrounded by the literal
# at-sign markers shown below; this comment uses different delimiters so
# `sed` doesn't rewrite the documentation block):
#   {VERSION}           the released version (no leading `v`)
#   {URL_DARWIN_ARM}    full GH Releases URL for aarch64-apple-darwin tarball
#   {SHA_DARWIN_ARM}    sha256 of that tarball
#   {URL_DARWIN_X86}    full GH Releases URL for x86_64-apple-darwin tarball
#   {SHA_DARWIN_X86}    sha256 of that tarball
#   {URL_LINUX_X86}     full GH Releases URL for x86_64-unknown-linux-gnu tarball
#   {SHA_LINUX_X86}     sha256 of that tarball
#   {URL_LINUX_ARM}     full GH Releases URL for aarch64-unknown-linux-gnu tarball
#   {SHA_LINUX_ARM}     sha256 of that tarball

class BrowserControl < Formula
  desc "CLI for browser lifecycle and CDP/BiDi access for agent-driven dev"
  homepage "https://github.com/rickardp/browser-control"
  version "1.0.3"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v1.0.3/browser-control-aarch64-apple-darwin.tar.gz"
      sha256 "c1644623dbd53d82f5be28092e9abdcaf7993a1e5a4f749d6152326cee288322"
    end
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v1.0.3/browser-control-x86_64-apple-darwin.tar.gz"
      sha256 "1e25a0f811c54632bf1d42d0b896d7858a83abee51d7356ad6255738eec8e105"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v1.0.3/browser-control-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "eace9411a896f5064079daefcb272184f1cbd75518edb97eba0765e9b56bf6d7"
    end
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v1.0.3/browser-control-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "512acced162413fc7800dceac57b824cc0420733a4e35552dfaf38464803b0a0"
    end
  end

  def install
    bin.install "browser-control"
  end

  test do
    assert_match "browser-control", shell_output("#{bin}/browser-control --version")
    # list-installed must run without a registry; exit code is best-effort
    # (CI runners typically have no browsers installed, which is allowed).
    system "#{bin}/browser-control", "list-installed"
  end
end
