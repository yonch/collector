# NRI Init Container
# This container checks and optionally configures NRI for containerd
FROM alpine:3.19

# Install required tools
RUN apk add --no-cache \
    bash \
    sed \
    grep \
    coreutils

# Create directory for scripts
RUN mkdir -p /scripts

# The actual script will be mounted from ConfigMap
# This image just provides the runtime environment

ENTRYPOINT ["/bin/bash"]
CMD ["/scripts/nri-init.sh"]