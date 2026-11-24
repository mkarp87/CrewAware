# End-to-end checks

Ad hoc scripts that drive a running CrewAware in headless Chrome (Playwright for Python) against the
fake radio. They are not part of `cargo test` or CI. Run them in order against a fresh database:

```
mkdir -p /tmp/e2e && python3 tests/e2e/make_overlay.py
cargo run -p crewaware-gateway --bin fake-radio -- --listen 127.0.0.1:7777 &     # FAKE_PARKED_SECS=60 by default
CREWAWARE_HTTP_BIND=127.0.0.1:18082 CREWAWARE_ADMIN_USERNAME=admin CREWAWARE_ADMIN_PASSWORD=adminpass1234 \
CREWAWARE_RADIO=tcp://127.0.0.1:7777 CREWAWARE_SECRET_KEY=$(openssl rand -hex 32) \
CREWAWARE_UNIT_POLL_SECS=20 CREWAWARE_REPEATER_POLL_SECS=20 DATABASE_URL=... cargo run -p crewaware &
python3 tests/e2e/01_setup.py && python3 tests/e2e/02_repeater_reads.py && python3 tests/e2e/03_routing.py \
  && python3 tests/e2e/04_obstacles_sites.py
```

`05_contact_loss.py` kills and restarts the fake radio itself and expects `/tmp/e2e/radio.sh` (starts
`fake-radio` with a given parked-advert interval in seconds). It takes about three minutes.
`01_setup.py` expects `/tmp/e2e/overlay.png`, the Chrome path `/usr/bin/google-chrome`, and port 18082.
