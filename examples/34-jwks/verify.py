# A service that holds nothing but the JWKS URL, as any relying party
# would: PyJWT fetches the set, picks the key by the token's kid, and
# checks the signature and expiry.
# example: omits error handling; PyJWT raises on a bad token
import sys
import jwt

token = open(sys.argv[1]).read().strip()
jwks = jwt.PyJWKClient("http://127.0.0.1:18499/.well-known/jwks.json")
key = jwks.get_signing_key_from_jwt(token)
claims = jwt.decode(token, key, algorithms=["EdDSA"])
print("kid", key.key_id, "sub", claims["sub"])
