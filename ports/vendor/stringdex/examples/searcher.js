(function() {
    const search = document.getElementById("search");
    const output = document.getElementById("output");
    const statsbar = document.getElementById("statsbar");
    const utf8decoder = new TextDecoder('utf-8');
    const utf8encoder = new TextEncoder('utf-8');
    async function addToRendered(entry, searchQueryString, database) {
        const name = utf8decoder.decode(await database.getData("word").at(entry));
        const dt = document.createElement("dt");
        dt.appendChild(document.createTextNode(name + "(id=" + entry + ")"));
        const definition = utf8decoder.decode(await database.getData("definition").at(entry));
        const dd = document.createElement("dd");
        dd.appendChild(document.createTextNode(definition));
        if (search.value === searchQueryString) {
            output.appendChild(dt);
            output.appendChild(dd);
        }
    }
    var db = null;
    function runSearch() {
        const searchQueryString = search.value;
        let databaseCallbacks = null;
        if (!db) {
            db = window.Stringdex.loadDatabase({
                loadRoot: callbacks => {
                    for (const key in callbacks) {
                        if (Object.hasOwn(callbacks, key)) {
                            // @ts-ignore
                            window[key] = callbacks[key];
                        }
                    }
                    databaseCallbacks = callbacks;
                    const script = document.createElement("script");
                    script.src = "search-index.js";
                    script.onerror = databaseCallbacks.err_rr_;
                    document.documentElement.appendChild(script);
                },
                loadTreeByHash: hashHex => {
                    const script = document.createElement("script");
                    script.src = `search.index/${hashHex}.js`;
                    script.onerror = databaseCallbacks.err_rr_;
                    document.documentElement.appendChild(script);
                },
                loadDataByNameAndHash: (_name, hashHex) => {
                    const script = document.createElement("script");
                    script.src = `search.data/${hashHex}.js`;
                    script.onerror = databaseCallbacks.err_rr_;
                    document.documentElement.appendChild(script);
                },
            });
        }
        db.then(async database => {
            if (search.value !== searchQueryString) {
                return;
            }
            output.innerHTML = "";
            statsbar.innerHTML = "";
            if (searchQueryString === "") {
                return;
            }
            const searchQuery = utf8encoder.encode(searchQueryString);
            const triestart = Date.now();
            const index = await database.getData("word");
            const trie = await index.search(searchQuery);
            const trieend = Date.now();
            let resultUnion = new window.RoaringBitmap(null);
            if (search.value !== searchQueryString) {
                return;
            }
            if (trie === null) {
                statsbar.innerHTML = `trie=${(trieend - triestart)/1000}s (none found)`;
            } else {
                statsbar.innerHTML = `trie=${(trieend - triestart)/1000}s `;
                const prefixstart = Date.now();
                let results = trie.prefixMatches();
                outer: for await (const result of results) {
                    for (const entry of result.entries()) {
                        if (resultUnion.cardinality() >= 50) {
                            break outer;
                        }
                        await addToRendered(entry, searchQueryString, database);
                    }
                    resultUnion = resultUnion.union(result);
                }
                const prefixend = Date.now();
                if (search.value !== searchQueryString) {
                    return;
                }
                statsbar.innerHTML += ` prefix=${(prefixend - prefixstart)/1000}s `;
            }
            if (resultUnion.cardinality() < 50 && searchQuery.length >= 3) {
                const levstart = Date.now();
                const levResults = await index.searchLev(searchQuery);
                for await (const levResult of levResults) {
                    for (const entry of levResult.matches().entries()) {
                        if (!resultUnion.contains(entry)) {
                            await addToRendered(entry, searchQueryString, database);
                        }
                    }
                    resultUnion = resultUnion.union(levResult.matches());
                    if (resultUnion.cardinality() >= 50) {
                        break;
                    }
                }
                const levend = Date.now();
                if (search.value !== searchQueryString) {
                    return;
                }
                statsbar.innerHTML += ` lev=${(levend - levstart)/1000}s `;
            }
            if (trie !== null) {
                const substringstart = Date.now();
                const substringResults = await trie.substringMatches();
                outer: for await (const result of substringResults) {
                    for (const entry of result.entries()) {
                        if (resultUnion.cardinality() >= 50) {
                            break outer;
                        }
                        if (!resultUnion.contains(entry)) {
                            await addToRendered(entry, searchQueryString, database);
                        }
                    }
                    resultUnion = resultUnion.union(result);
                }
                const substringend = Date.now();
                if (search.value !== searchQueryString) {
                    return;
                }
                statsbar.innerHTML += ` substring=${(substringend - substringstart)/1000}s total=${(substringend - triestart)/1000}s`;
            }
        });
    }
    let debounce = null;
    search.addEventListener("change", runSearch);
    search.addEventListener("keyup", () => {
        if (debounce !== null) {
            clearTimeout(debounce);
        }
        debounce = setTimeout(() => {
            debounce = null;
            runSearch();
        }, 250);
    });
    search.addEventListener("cut", runSearch);
    search.addEventListener("paste", runSearch);
    if (search.value !== "") {
        debounce = setTimeout(() => {
            debounce = null;
            runSearch();
        }, 250);
    }
})();
