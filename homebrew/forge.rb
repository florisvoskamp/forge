# Homebrew formula for Forge.
#
#   brew tap florisvoskamp/forge https://github.com/florisvoskamp/forge
#   brew install forge
#
# version + sha256 below are filled in per release (the checksums.txt asset
# produced by .github/workflows/release.yml has the values). Until then, the
# `curl | sh` installer (install.sh) is the recommended path.
class Forge < Formula
  desc "Multi-provider mesh AI coding CLI"
  homepage "https://github.com/florisvoskamp/forge"
  version "1.8.3" # auto-updated by release.yml (scripts/update-brew-formula.sh) per release
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/florisvoskamp/forge/releases/download/v#{version}/forge-aarch64-apple-darwin.tar.gz"
      sha256 "7526b9ae4f4d68a964aa74ae0e130e2680913143f86f45998a86d89aeb3b300a"
    end
    on_intel do
      url "https://github.com/florisvoskamp/forge/releases/download/v#{version}/forge-x86_64-apple-darwin.tar.gz"
      sha256 "a80ae8b25856c80030b888d00212bb5acc4ab375376bfa7e4a460f1960e60cce"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/florisvoskamp/forge/releases/download/v#{version}/forge-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "4e267e49a857ba4d14b8dc24f57fe26a1c8bdaa0f98b71184b04f291e29a0254"
    end
  end

  def install
    bin.install "forge"
  end

  test do
    assert_match "forge", shell_output("#{bin}/forge --version")
  end
end
