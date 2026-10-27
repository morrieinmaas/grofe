#!/bin/sh
# A default-shell that logs how it was started, then runs /bin/sh as given,
# for the FE.3 default-shell golden.
echo "probe $*" >> "$HOME/probe.log"
exec /bin/sh "$@"
