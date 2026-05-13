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
# Placeholders rendered by the workflow:
#   0.2.0           the released version (no leading `v`)
#   https://github.com/rickardp/browser-control/releases/download/v0.2.0/browser-control-aarch64-apple-darwin.tar.gz    full GH Releases URL for aarch64-apple-darwin tarball
#   f453ba31a3e9db8c1b20ef21ac6fa65b6275a236e2e2645c60e0ac898b26aa82    sha256 of that tarball
#   https://github.com/rickardp/browser-control/releases/download/v0.2.0/browser-control-x86_64-apple-darwin.tar.gz    full GH Releases URL for x86_64-apple-darwin tarball
#   90a4cc3e6db5afd3f959e0a249e5095e4c787ecb634bb6decf919a28c9cf8ac8    sha256 of that tarball
#   https://github.com/rickardp/browser-control/releases/download/v0.2.0/browser-control-x86_64-unknown-linux-gnu.tar.gz     full GH Releases URL for x86_64-unknown-linux-gnu tarball
#   9bd033abfbf2c9a355315400a827952ac3fa1aab3273bbaeea4b62c252b4fd08     sha256 of that tarball
#   https://github.com/rickardp/browser-control/releases/download/v0.2.0/browser-control-aarch64-unknown-linux-gnu.tar.gz     full GH Releases URL for aarch64-unknown-linux-gnu tarball
#   a01e7e19d3ca0787232e2f5d5c65c7029b9e53a68d9ed51a96165f30f0ded1b2     sha256 of that tarball

class BrowserControl < Formula
  desc "CLI for browser lifecycle and CDP/BiDi access for agent-driven dev"
  homepage "https://github.com/rickardp/browser-control"
  version "0.2.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v0.2.0/browser-control-aarch64-apple-darwin.tar.gz"
      sha256 "f453ba31a3e9db8c1b20ef21ac6fa65b6275a236e2e2645c60e0ac898b26aa82"
    end
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v0.2.0/browser-control-x86_64-apple-darwin.tar.gz"
      sha256 "90a4cc3e6db5afd3f959e0a249e5095e4c787ecb634bb6decf919a28c9cf8ac8"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v0.2.0/browser-control-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "9bd033abfbf2c9a355315400a827952ac3fa1aab3273bbaeea4b62c252b4fd08"
    end
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v0.2.0/browser-control-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "a01e7e19d3ca0787232e2f5d5c65c7029b9e53a68d9ed51a96165f30f0ded1b2"
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
