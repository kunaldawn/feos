const fs = require('fs');

/** @type {[string, function(any, Buffer<ArrayBufferLike>?): any][]} */
const workQueue = [];

if (!fs.existsSync("rng-seed.txt")) {
    fs.writeFileSync("rng-seed.txt", ((Math.random() * 0x7FFF_FFFF) | 0).toString());
}

// Is there a built-in way to do seeded random scheduling in Node,
// like Elixir's test harness supports OOTB?
//
// I really hope there is, because (1) I'm not convinced that this finds
// all possible races (2) This is slow.
//
// I'm willing to pull in external dependencies for testing, even though
// I try to avoid them for code I'm distributing.
let myRngSeed = Number(fs.readFileSync("rng-seed.txt").toString("utf8"));
const myRng = () => {
    // https://en.wikipedia.org/wiki/Xorshift
    let x = myRngSeed;
	x ^= x << 13;
	x ^= x >> 17;
	x ^= x << 5;
    myRngSeed = x;
    return x & 0x7FFF_FFFF;
};

const stepWorkQueue = () => {
    while (workQueue.length !== 0) {
        const i = myRng() % workQueue.length;
        const [path, job] = workQueue[i];
        workQueue[i] = workQueue[workQueue.length - 1];
        workQueue.length -= 1;
        try {
            job(null, fs.readFileSync(path));
        } catch (e) {
            job(e, null);
        }
    }
};
process.nextTick(stepWorkQueue);

/**
 * @import * as stringdex from "./stringdex.d.ts"
 */
const {Stringdex, RoaringBitmap} = require('./stringdex.js');

function usage() {
    console.log("Usage: node nodesearch.js (exact|lev|substring|prefix) [column]");
    console.log("       node nodesearch.js lookup [key-column] [value-column]");
    process.exit(1);
}

if (process.env['STRINGDEX_JS_SEARCH_BREAK']) {
    process.stdout.write("1\n");
}

const searchKind = process.argv[2];

var searchQuery = fs.readFileSync(0);

/** @type {stringdex.Callbacks?} */
let databaseCallbacks = null;
const database = Stringdex.loadDatabase({
    loadRoot: callbacks => {
        for (const key in callbacks) {
            if (Object.hasOwn(callbacks, key)) {
                // @ts-ignore
                global[key] = callbacks[key];
            }
            databaseCallbacks = callbacks;
            workQueue.push([
                `search-index.js`,
                (err, data) => {
                    if (data) {
                        eval(data.toString("utf8"));
                    } else if (databaseCallbacks) {
                        databaseCallbacks.err_rr_(err);
                    }
                },
            ]);
            process.nextTick(stepWorkQueue);
        }
    },
    loadTreeByHash: hashHex => {
        workQueue.push([
            `search.index/${hashHex}.js`,
            (err, data) => {
                if (data) {
                    eval(data.toString("utf8"));
                } else if (databaseCallbacks) {
                    databaseCallbacks.err_rn_(hashHex, err);
                }
            },
        ]);
        process.nextTick(stepWorkQueue);
    },
    loadDataByNameAndHash: (name, hashHex) => {
        workQueue.push([
            `search.data/${name}/${hashHex}.js`,
            (err, data) => {
                if (data) {
                    eval(data.toString("utf8"));
                } else if (databaseCallbacks) {
                    databaseCallbacks.err_rd_(hashHex, err);
                }
            },
        ]);
        process.nextTick(stepWorkQueue);
    },
});

// perform search, write output
database.then(async database => {
    /** @type {stringdex.RoaringBitmap} */
    let results = new RoaringBitmap(null);
    if (process.argv.length < 4) {
        usage();
    }
    const dataColumn = database.getData(process.argv[3]);
    if (dataColumn === undefined) {
        throw new Error("failed to load search tree");
    }
    const trie = await dataColumn.search(searchQuery);
    switch (searchKind) {
        case "exact":
        case "prefix":
        case "substring":
            if (process.argv.length !== 4) {
                usage();
            }
            if (trie) {
                if (searchKind === "exact") {
                    results = trie.matches();
                } else {
                    const orderedset = searchKind === "substring" ? trie.substringMatches() : trie.prefixMatches();
                    for await (const set of orderedset) {
                        results = results.union(set);
                    }
                }
            }
            break;
        case "lev":
            if (process.argv.length !== 4) {
                usage();
            }
            const orderedset = dataColumn.searchLev(searchQuery);
            for await (const set of orderedset) {
                results = results.union(set.matches());
            }
            break;
        case "lookup":
            if (process.argv.length !== 5) {
                usage();
            }
            if (trie) {
                results = trie.matches();
            }
            const data = database.getData(process.argv[4]);
            if (!data) {
                throw new Error("failed to load data");
            }
            for (const id of results.entries()) {
                const d = await data.at(id);
                if (d === undefined) {
                    throw new Error(`weird ${id} missing`);
                }
                process.stdout.write(uint8ArrayToBase64(d));
                process.stdout.write("\n");
            }
            return;
        default:
            usage();
            return;
    }
    for (const result of results.entries()) {
        process.stdout.write(result.toString());
        process.stdout.write("\n");
    }
}, console.log);

// https://github.com/tc39/proposal-arraybuffer-base64/blob/main/playground/polyfill-core.mjs
/**
 * 
 * @param {Uint8Array} arr 
 * @returns {string}
 */
function uint8ArrayToBase64(arr) {
    let lookup = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
    let result = '';
  
    let i = 0;
    for (; i + 2 < arr.length; i += 3) {
      let triplet = (arr[i] << 16) + (arr[i + 1] << 8) + arr[i + 2];
      result +=
        lookup[(triplet >> 18) & 63] +
        lookup[(triplet >> 12) & 63] +
        lookup[(triplet >> 6) & 63] +
        lookup[triplet & 63];
    }
    if (i + 2 === arr.length) {
      let triplet = (arr[i] << 16) + (arr[i + 1] << 8);
      result +=
        lookup[(triplet >> 18) & 63] +
        lookup[(triplet >> 12) & 63] +
        lookup[(triplet >> 6) & 63] +
        '=';
    } else if (i + 1 === arr.length) {
      let triplet = arr[i] << 16;
      result +=
        lookup[(triplet >> 18) & 63] +
        lookup[(triplet >> 12) & 63] +
        '==';
    }
    return result;
  }
