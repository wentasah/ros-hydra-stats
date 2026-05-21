# ROS Hydra Stats

Tool to scrape results of [Hydra][] builds for [nix-ros-overlay][] and
produce human readable and maintainer actionable summary of build and
evaluation failures.

Currently, this is intended to work just for me. Not much effort is
spent to make the tool usable by others. However, over time, it may
evolve into a generic tool or even automatic Github App.

## Example output

Examples of the output can be seen in nix-ros-overlay PRs such as
[here][example].

[Hydra]: https://hydra.iid.ciirc.cvut.cz/
[nix-ros-overlay]: https://github.com/lopsided98/nix-ros-overlay
[example]: https://github.com/lopsided98/nix-ros-overlay/pull/853#issuecomment-4502381645
