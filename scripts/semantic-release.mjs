import { appendFile } from "node:fs/promises";

import semanticRelease from "semantic-release";

const args = new Set(process.argv.slice(2));
const dryRun = args.has("--dry-run");
const noCi = args.has("--no-ci");
const expectedVersion = process.env.EXPECTED_VERSION;

if (!dryRun && !expectedVersion) {
  throw new Error("EXPECTED_VERSION is required when publishing a release.");
}

const result = await semanticRelease({ dryRun, ci: !noCi });
const nextRelease = result?.nextRelease;
const publish = Boolean(nextRelease);
const version = nextRelease?.version ?? "";
const channel = nextRelease?.channel ?? "";

if (!dryRun && (!publish || version !== expectedVersion)) {
  throw new Error(
    `semantic-release produced ${version || "no release"}; expected ${expectedVersion}.`,
  );
}

console.log(`publish=${publish}`);
console.log(`version=${version}`);
console.log(`channel=${channel}`);

if (process.env.GITHUB_OUTPUT) {
  await appendFile(
    process.env.GITHUB_OUTPUT,
    `publish=${publish}\nversion=${version}\nchannel=${channel}\n`,
  );
}
