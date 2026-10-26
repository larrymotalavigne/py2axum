- Several route decorators on one endpoint (`@app.get("/a")` over `@app.post("/b")`): one route each, the lowest
  decorator registered first, each with its own options.
