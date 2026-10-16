"""A module whose variable another module rebinds (`conf.api_key = ...`, as `stripe.api_key = ...`)."""
api_key = None
calls = 0


def current():
    return api_key
