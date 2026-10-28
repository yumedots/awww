PREFIX ?= /usr
CARGO ?= cargo
BIN := target/release
BINS := awww awww-daemon

.PHONY: all build install uninstall clean

all: build

build:
	$(CARGO) build --release -p awww-daemon -p awww

install: build
	for bin in $(BINS); do install -Dm755 $(BIN)/$$bin $(DESTDIR)$(PREFIX)/bin/$$bin; done

uninstall:
	for bin in $(BINS); do rm -f $(DESTDIR)$(PREFIX)/bin/$$bin; done

clean:
	$(CARGO) clean
