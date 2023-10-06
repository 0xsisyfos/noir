#!/bin/bash

# Check if working directory is clean
if [ -n "$(git status --porcelain)" ]; then
  echo "Working directory is not clean, please commit or stash your changes before publishing"
  exit 1
fi

# Run yarn and yarn build so that the built artifacts are up to date;
# They should now reference the changed scope package names 
yarn && yarn build

# update the versions in the workspace package.json files
# so that they become version-{branch-name}-{commit-hash}
bash ./scripts/update-version.sh || { echo "Update version script failed"; exit 1; }

# publish and patch the workspace dependencies
bash ./scripts/publish-script.sh || { echo "Publish script failed"; exit 1; }

# The above processes will modify a lot of files
# Lets reset the changes that were made
git restore --source=HEAD --staged --worktree -- .