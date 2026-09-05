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
  version "1.3.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v1.3.0/browser-control-aarch64-apple-darwin.tar.gz"
      sha256 "8733933b3357944daf822d3f357cce5e57e4e437897eddc8b8509029d17adb9d"
    end
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v1.3.0/browser-control-x86_64-apple-darwin.tar.gz"
      sha256 "d4d151a76b6a62377f872e1719d2c17d84661abfeb28e8b78d776e5aa4516047"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v1.3.0/browser-control-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "3d6be6996d9b2e0f12773d114559f0cc446a4daf33679c221a1a3103cb70eec6"
    end
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v1.3.0/browser-control-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "b7c3993784830766c011d5e06959db6accc8c438f519dc146f7892cfa8040f38"
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
