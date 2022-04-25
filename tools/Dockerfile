FROM debian:bullseye
RUN apt update \
    && apt install -y git xorriso  build-essential
VOLUME ["/user_data"]

CMD tail -f /dev/null