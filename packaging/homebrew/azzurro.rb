# Homebrew cask for Azzurro.
#
# This file exists twice and update-cask.sh keeps the two in step: the template
# at packaging/homebrew/azzurro.rb in the app repo, which versions with the thing
# it describes, and the published cask at github.com/jzbz/homebrew-tap in
# Casks/azzurro.rb, which is that template with the release's version and hash
# filled in.
#
#   brew install --cask jzbz/tap/azzurro
#
# A tap rather than homebrew-cask because homebrew-cask applies a notability
# bar, and at 3x for a self-submission that is 225 stars, 90 forks or 90
# watchers, plus a 30-day repository age. A tap has none of that. What a tap
# does NOT escape is Gatekeeper: brew applies com.apple.quarantine on install
# whatever tap a cask came from, --no-quarantine was removed in Homebrew 4.7,
# and the `quarantine` stanza no longer exists in the DSL. So the notarized
# zip is what makes this work — an unsigned one would install and then refuse
# to open, which is worse than not offering it.
cask "azzurro" do
  version "0.1.0"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"

  url "https://github.com/jzbz/azzurro/releases/download/v#{version}/azzurro-v#{version}-macos-universal.zip",
      verified: "github.com/jzbz/azzurro/"
  name "Azzurro"
  desc "Control BluOS players"
  homepage "https://azzurro.blue/"

  livecheck do
    url :url
    strategy :github_latest
  end

  # Matches LSMinimumSystemVersion in the bundle's Info.plist. Both slices of
  # the universal binary are built against 11.0.
  depends_on macos: ">= :big_sur"

  app "Azzurro.app"

  # Complete, unlike the cask this was modeled on. rPGP holds secret keys and
  # so had to leave its store behind; nothing here is irreplaceable. The four
  # config files are a list of players seen, saved searches, custom stations
  # and the sidebar order, and the cache is downloaded cover art — all of it
  # rebuilt by using the app, and none of it worth surprising someone with by
  # leaving it on disk after they asked for everything to go.
  zap trash: [
    "~/Library/Application Support/azzurro",
    "~/Library/Caches/azzurro",
    "~/Library/Saved Application State/blue.azzurro.Azzurro.savedState",
  ]
end
