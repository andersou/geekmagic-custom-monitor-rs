const conventionalCommits = {
  preset: "conventionalcommits",
  presetConfig: {},
};

const releaseRules = [
  { type: "feat", release: "minor" },
  { type: "fix", release: "patch" },
  { type: "perf", release: "patch" },
  { type: "revert", release: "patch" },
  { type: "docs", release: false },
  { type: "chore", release: false },
  { type: "ci", release: false },
  { type: "test", release: false },
  { type: "refactor", release: false },
  { type: "style", release: false },
  { type: "build", release: false },
];

export default {
  tagFormat: "v${version}",
  branches: [
    { name: "main" },
    { name: "develop", channel: "beta", prerelease: "beta" },
  ],
  plugins: [
    ["@semantic-release/commit-analyzer", { ...conventionalCommits, releaseRules }],
    ["@semantic-release/release-notes-generator", conventionalCommits],
    [
      "@semantic-release/exec",
      {
        verifyReleaseCmd:
          'if [ -n "$EXPECTED_VERSION" ] && [ "$EXPECTED_VERSION" != "${nextRelease.version}" ]; then printf "%s\\n" "planned version $EXPECTED_VERSION does not match ${nextRelease.version}" >&2; exit 1; fi',
        prepareCmd:
          "cargo set-version ${nextRelease.version} && cargo metadata --locked --no-deps",
      },
    ],
    [
      "@semantic-release/git",
      {
        assets: ["Cargo.toml", "Cargo.lock"],
        message: "chore(release): ${nextRelease.version} [skip ci]",
      },
    ],
    [
      "@semantic-release/github",
      {
        assets: ["dist/*.tar.gz", "dist/*.zip", "dist/SHA256SUMS"],
        successComment: false,
        failComment: false,
        failTitle: false,
        labels: false,
        releasedLabels: false,
      },
    ],
  ],
};
