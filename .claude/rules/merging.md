# Merging

These repositories are public. A pull request does not merge while any
of its checks is failing, cancelled or still running. That includes:

- a check GitHub cancelled, such as a runner that never arrived or a
  job that hit its time limit;
- a failing pull request run whose push run passed on the same commit.
  The run the pull request shows is the one that counts.

When a check is red:

1. Find the cause and fix it. If the cause is outside the code (runner
   capacity, a time limit), rerun the check and wait for it to pass.
2. Do not tell the owner a red pull request is safe to merge, probably
   fine, or only a duplicate. Say what is red and what is being done
   about it.
3. If it cannot go green, the pull request waits. Tell the owner what
   is blocking it.

A red check on `main` is handled the same way, before anything else
merges.
