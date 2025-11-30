#!/bin/sh
#
# Copyright © 2023 Guillem Jover <guillem@debian.org>
#
# This program is free software; you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation; either version 2 of the License, or
# (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU General Public License for more details.
#
# You should have received a copy of the GNU General Public License
# along with this program.  If not, see <https://www.gnu.org/licenses/>.

PROGNAME=$(basename "$0")
ADMINDIR='/var/lib/dpkg'

PKGDATADIR_DEFAULT='/usr/share/dpkg'
PKGDATADIR="${DPKG_DATADIR:-$PKGDATADIR_DEFAULT}"

# shellcheck source=src/sh/dpkg-error.sh
. "$PKGDATADIR/sh/dpkg-error.sh"

setup_colors

# This script is used to track any change in the dpkg database for later
# inspection, either as a matter of record tracking or to aid in debugging
# dpkg behavior. It can be installed as a post-invoke hook such as:
#
# ,--- /etc/dpkg/dpkg.cfg.d/db-keeper ---
# post-invoke "/usr/libexec/dpkg-db-keeper"
# `---

if [ -n "$DPKG_ROOT" ]; then
  # Only operate on the main system.
  exit 0
fi

if ! command -v git >/dev/null; then
  # Do nothing if there is no git.
  exit 0
fi

: "${DPKG_ADMINDIR:=/var/lib/dpkg}"

cd "$DPKG_ADMINDIR" || exit 0

if [ ! -e .git ]; then
  # Initialize the git repo.
  git init
fi

git add -A
git commit -m 'Commit dpkg db updates'

exit 0
