---
title: Authorization Code Flow
topic: authentication
category: reference
tags: [oauth, tokens, redirect]
date: 2026-05-16
---

## The redirect

The client sends the person to the authorization endpoint with its
identifier, the requested scopes, a redirect target, and a random state
value. After they approve, the browser comes back to the redirect target
carrying a short-lived code. Reject the response if the state does not
match what you sent; that check is what stops a forged callback.

## Trading the code

The code is single-use and expires in sixty seconds. The client posts it
to the token endpoint together with its secret and the same redirect
target, and receives an access token plus a refresh token. Do this from
the server side. A code redeemed from a browser leaks the secret to
anybody reading the page source.

## Renewal

Access tokens live for one hour. Use the refresh token to obtain a new
one before expiry rather than after a failure, and store refresh tokens
encrypted at rest, since they are as good as a password until revoked.
