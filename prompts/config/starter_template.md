# Model configuration for tinker. Uncomment and edit a slot to override
# the built-in default for that tier. Commenting a line out reverts to
# the built-in default. Unrecognized endpoint URLs or model identifiers
# pass through to the backend as-is — the config governs choice only,
# not validation.
#
# Each tier has two fields:
#   endpoint — the OpenAI-protocol chat-completions URL
#   model    — the model identifier the endpoint serves
#
# Auth is sourced from per-tier environment variables — never put API
# keys in this file. An unset/empty auth value means no Authorization
# header is sent, which is what local model servers expect.

# high — used by tend, rummage, jog (the strongest tier).
# Auth env var: TINKER_HIGH_API_KEY
[native.high]
# endpoint = "{NATIVE_HIGH_ENDPOINT}"  # built-in default
# model    = "{NATIVE_HIGH_MODEL}"     # built-in default

# mid — used by goal sessions.
# Auth env var: TINKER_MID_API_KEY
[native.mid]
# endpoint = "{NATIVE_MID_ENDPOINT}"  # built-in default
# model    = "{NATIVE_MID_MODEL}"     # built-in default

# low — used by cleanup (the cheapest tier).
# Auth env var: TINKER_LOW_API_KEY
[native.low]
# endpoint = "{NATIVE_LOW_ENDPOINT}"  # built-in default
# model    = "{NATIVE_LOW_MODEL}"     # built-in default