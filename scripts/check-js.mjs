import { spawnSync } from "node:child_process";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { parse } from "acorn";

const projectRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const webRoot = join(projectRoot, "clients", "web");
const sourceFiles = [
  ...walk(webRoot),
  ...walk(join(projectRoot, "tests", "frontend")),
  join(projectRoot, "playwright.config.js"),
  fileURLToPath(import.meta.url),
].filter(path => /\.(?:js|mjs)$/u.test(path)).sort();
const failures = [];
const DOM_SINK_NAMES = new Set([
  "DOMParser",
  "createContextualFragment",
  "innerHTML",
  "insertAdjacentHTML",
  "outerHTML",
  "setHTMLUnsafe",
  "write",
  "writeln",
]);
const EVALUATION_NAMES = new Set(["Function", "eval"]);
const NATIVE_MODAL_NAMES = new Set(["alert", "confirm", "prompt"]);
const GLOBAL_OBJECT_NAMES = new Set([
  "document",
  "globalThis",
  "self",
  "window",
]);
const STATIC_VALUE_LIMIT = 32;
const STATIC_STRING_LENGTH_LIMIT = 512;

const detectionFixtures = [
  [
    "clients/web/modules/listing/controller.js",
    "const request = fetch; request('/');\n",
    "fetch",
  ],
  [
    "clients/web/modules/listing/controller.js",
    "const Transport = XMLHttpRequest; new Transport();\n",
    "XMLHttpRequest",
  ],
  [
    "clients/web/modules/listing/controller.js",
    "document.createRange().createContextualFragment('<p>unsafe</p>');\n",
    "createContextualFragment",
  ],
  [
    "clients/web/modules/listing/controller.js",
    "element['inner' /* split */ + 'HTML'] = userControlled;\n",
    "innerHTML",
  ],
  [
    "clients/web/modules/listing/controller.js",
    "element[`inner${'HTML'}`] = userControlled;\n",
    "innerHTML",
  ],
  [
    "clients/web/modules/listing/controller.js",
    "const key = ['inner', 'HTML'].join(''); element[key] = value;\n",
    "innerHTML",
  ],
  [
    "clients/web/modules/listing/controller.js",
    "const prefix = 'inner'; const key = `${prefix}HTML`; element[key] = value;\n",
    "innerHTML",
  ],
  [
    "clients/web/modules/listing/controller.js",
    "globalThis['ev\\u0061l'](userControlled);\n",
    "eval",
  ],
  [
    "clients/web/modules/listing/controller.js",
    "const show = globalThis.alert; show(userControlled);\n",
    "alert",
  ],
  [
    "clients/web/modules/listing/controller.js",
    "window['con' + 'firm'](userControlled);\n",
    "confirm",
  ],
  [
    "clients/web/modules/listing/controller.js",
    "Reflect.get(window, 'prompt')(userControlled);\n",
    "prompt",
  ],
  [
    "clients/web/modules/listing/controller.js",
    "const root = globalThis; const key = ['fe', 'tch'].join(''); root[key]('/');\n",
    "fetch",
  ],
  [
    "clients/web/modules/listing/controller.js",
    "const key = getName(); globalThis[key]('/');\n",
    "dynamic global property access",
  ],
  [
    "clients/web/modules/listing/controller.js",
    "const key = getName(); element[key] = userControlled;\n",
    "dynamic computed property write",
  ],
  [
    "clients/web/modules/listing/controller.js",
    "const key = getName(); const {[key]: fn} = globalThis; fn(input);\n",
    "dynamic global destructuring property",
  ],
  [
    "clients/web/modules/listing/controller.js",
    "const key = getName(); function f({[key]: fn} = globalThis) { fn(input); } f();\n",
    "dynamic global destructuring property",
  ],
  [
    "clients/web/modules/listing/controller.js",
    "const key = getName(); const root = globalThis; function f({nested: {[key]: fn}} = root) { fn(input); } f();\n",
    "dynamic global destructuring property",
  ],
  [
    "clients/web/modules/listing/controller.js",
    "const key = getName(); function f({[key]: fn}) { fn(input); } f(globalThis);\n",
    "dynamic computed destructuring property",
  ],
];
for (const [name, source, expected] of detectionFixtures) {
  if (!productionSafetyIssues(name, source).some(issue =>
    issue.includes(expected)
  )) {
    failures.push(
      `scripts/check-js.mjs: AST safety fixture did not detect ${expected}`,
    );
  }
}

const safeFixtures = [
  [
    "clients/web/modules/listing/controller.js",
    [
      "// innerHTML, eval, fetch and XMLHttpRequest are inert comments.",
      "const label = 'innerHTML and eval and fetch';",
      "const selected = items[index];",
      "items[index] = null;",
      "console.log(label, selected);",
      "",
    ].join("\n"),
  ],
  [
    "clients/web/modules/http/client.js",
    "const request = fetch; request('/');\n",
  ],
  [
    "clients/web/modules/upload/transport.js",
    "const Transport = XMLHttpRequest; new Transport();\n",
  ],
];
for (const [name, source] of safeFixtures) {
  const issues = productionSafetyIssues(name, source);
  if (issues.length > 0) {
    failures.push(
      `scripts/check-js.mjs: AST safety fixture produced a false positive: ` +
        issues.join("; "),
    );
  }
}

for (const path of sourceFiles) {
  const name = relative(projectRoot, path);
  const source = readFileSync(path, "utf8");
  checkSourceFormat(name, source);
  checkProductionSafety(name, source);

  const syntax = spawnSync(process.execPath, ["--check", path], {
    cwd: projectRoot,
    encoding: "utf8",
  });
  if (syntax.status !== 0) {
    failures.push(
      `${name}: JavaScript syntax check failed\n${syntax.stderr.trim()}`,
    );
  }
}

const packagePath = join(projectRoot, "package.json");
const packageSource = readFileSync(packagePath, "utf8");
const normalizedPackage = `${JSON.stringify(JSON.parse(packageSource), null, 2)}\n`;
if (packageSource !== normalizedPackage) {
  failures.push("package.json: expected deterministic two-space JSON formatting");
}

checkEmbeddedModules();

if (failures.length > 0) {
  process.stderr.write(`${failures.join("\n")}\n`);
  process.exit(1);
}
process.stdout.write(
  `JavaScript syntax, source formatting, and AST browser safety checks passed ` +
    `for ${sourceFiles.length} files\n`,
);

function walk(root) {
  const output = [];
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const path = join(root, entry.name);
    if (entry.isDirectory()) {
      output.push(...walk(path));
    } else if (entry.isFile() || statSync(path).isFile()) {
      output.push(path);
    }
  }
  return output;
}

function checkEmbeddedModules() {
  const modulesRoot = join(webRoot, "modules");
  const moduleNames = new Set(
    walk(modulesRoot)
      .filter(path => path.endsWith(".js"))
      .map(path => relative(webRoot, path)
        .split(sep).join("/")),
  );
  const registryPath = join(projectRoot, "src", "server", "assets.rs");
  const registrySource = readFileSync(registryPath, "utf8");
  const registeredNames = new Set(
    [...registrySource.matchAll(/name: "(modules\/[^"]+\.js)"/gu)]
      .map(match => match[1]),
  );

  for (const name of moduleNames) {
    if (!registeredNames.has(name)) {
      failures.push(
        `src/server/assets.rs: production module is not embedded (${name})`,
      );
    }
  }
  for (const name of registeredNames) {
    if (!moduleNames.has(name)) {
      failures.push(
        `src/server/assets.rs: embedded module does not exist (${name})`,
      );
    }
  }
}

function checkSourceFormat(name, source) {
  if (source.startsWith("\uFEFF")) {
    failures.push(`${name}: UTF-8 BOM is not allowed`);
  }
  if (source.includes("\r")) {
    failures.push(`${name}: use LF line endings`);
  }
  if (source.includes("\t")) {
    failures.push(`${name}: tabs are not allowed`);
  }
  if (!source.endsWith("\n")) {
    failures.push(`${name}: file must end with a newline`);
  }
  source.split("\n").forEach((line, index) => {
    if (/[ \t]+$/u.test(line)) {
      failures.push(`${name}:${index + 1}: trailing whitespace`);
    }
  });
}

function checkProductionSafety(name, source) {
  for (const issue of productionSafetyIssues(name, source)) {
    failures.push(`${name}: ${issue}`);
  }
}

function productionSafetyIssues(name, source) {
  if (!name.startsWith("clients/web/") || !name.endsWith(".js")) return [];

  let ast;
  try {
    ast = parse(source, {
      allowHashBang: true,
      ecmaVersion: "latest",
      locations: true,
      sourceType: "module",
    });
  } catch (error) {
    return [
      `JavaScript AST parse failed: ${error.message}`,
    ];
  }

  const model = buildLexicalModel(ast);
  const parents = new WeakMap();
  walkAst(ast, (node, parent) => {
    if (parent) parents.set(node, parent);
  });
  const issues = new Map();
  const addIssue = (node, message) => {
    const line = node.loc?.start.line || 1;
    issues.set(`${line}:${message}`, `line ${line}: ${message}`);
  };
  const reportName = (node, propertyName) => {
    if (DOM_SINK_NAMES.has(propertyName)) {
      addIssue(
        node,
        `dynamic HTML parsing or injection API is forbidden (${propertyName})`,
      );
    }
    if (EVALUATION_NAMES.has(propertyName)) {
      addIssue(
        node,
        `dynamic JavaScript evaluation is forbidden (${propertyName})`,
      );
    }
    if (NATIVE_MODAL_NAMES.has(propertyName)) {
      addIssue(
        node,
        `browser-native modal API is forbidden (${propertyName})`,
      );
    }
    if (
      propertyName === "fetch" &&
      name.startsWith("clients/web/modules/") &&
      name !== "clients/web/modules/http/client.js"
    ) {
      addIssue(node, "fetch must go through modules/http/client.js");
    }
    if (
      propertyName === "XMLHttpRequest" &&
      name.startsWith("clients/web/modules/") &&
      name !== "clients/web/modules/upload/transport.js"
    ) {
      addIssue(
        node,
        "XMLHttpRequest is restricted to the upload transport",
      );
    }
  };

  walkAst(ast, (node, parent, key) => {
    const scope = model.nodeScopes.get(node) || model.rootScope;
    if (
      node.type === "Identifier" &&
      isReferenceIdentifier(node, parent, key, model.bindingIdentifiers)
    ) {
      reportName(node, node.name);
    }

    if (node.type === "MemberExpression") {
      const properties = evaluatePropertyNames(
        node,
        scope,
        model,
      );
      if (properties === null) {
        if (globalObjectKind(node.object, scope, model) !== null) {
          addIssue(
            node,
            "dynamic global property access is forbidden",
          );
        }
        if (isDynamicCallOrWrite(node, parent, key)) {
          addIssue(
            node,
            parent?.type === "AssignmentExpression"
              ? "dynamic computed property write is forbidden"
              : "dynamic computed method invocation is forbidden",
          );
        }
      } else {
        for (const propertyName of properties) {
          reportName(node, propertyName);
        }
      }
    }

    if (
      node.type === "Property" &&
      parent?.type === "ObjectPattern"
    ) {
      const properties = node.computed
        ? evaluateStaticStrings(node.key, scope, model)
        : propertyNameFromSyntax(node.key);
      if (properties !== null) {
        for (const propertyName of properties) {
          reportName(node, propertyName);
        }
      } else if (node.computed) {
        addIssue(
          node,
          objectPatternUsesGlobal(node, parents, model)
            ? "dynamic global destructuring property is forbidden"
            : "dynamic computed destructuring property is forbidden",
        );
      }
    }

    if (node.type === "CallExpression" || node.type === "NewExpression") {
      checkReflectivePropertyOperation(
        node,
        scope,
        model,
        reportName,
        addIssue,
      );
      if (
        node.callee.type === "MemberExpression" &&
        evaluatePropertyNames(node.callee, scope, model)?.has("constructor")
      ) {
        addIssue(
          node,
          "indirect Function construction through .constructor is forbidden",
        );
      }
    }
  });

  return [...issues.values()];
}

function objectPatternUsesGlobal(property, parents, model) {
  for (let current = property; current; current = parents.get(current)) {
    const parent = parents.get(current);
    if (!parent) return false;
    if (parent.type === "VariableDeclarator" && parent.id === current) {
      const scope = model.nodeScopes.get(parent.init) || model.rootScope;
      return parent.init && globalObjectKind(parent.init, scope, model) !== null;
    }
    if (parent.type === "AssignmentExpression" && parent.left === current) {
      const scope = model.nodeScopes.get(parent.right) || model.rootScope;
      return globalObjectKind(parent.right, scope, model) !== null;
    }
    if (parent.type === "AssignmentPattern" && parent.left === current) {
      const scope = model.nodeScopes.get(parent.right) || model.rootScope;
      return globalObjectKind(parent.right, scope, model) !== null;
    }
    if (
      parent.type.endsWith("Statement") ||
      parent.type.endsWith("Declaration") ||
      parent.type === "Program"
    ) {
      return false;
    }
  }
  return false;
}

function buildLexicalModel(ast) {
  const nodeScopes = new WeakMap();
  const bindingIdentifiers = new WeakSet();
  const rootScope = createScope(null, "function");

  function createScope(parent, kind) {
    return { bindings: new Map(), kind, parent };
  }

  function nearestFunctionScope(scope) {
    let current = scope;
    while (current.parent && current.kind !== "function") {
      current = current.parent;
    }
    return current;
  }

  function define(scope, identifier, kind, init) {
    bindingIdentifiers.add(identifier);
    nodeScopes.set(identifier, scope);
    const existing = scope.bindings.get(identifier.name);
    if (existing) {
      existing.ambiguous = true;
      return;
    }
    scope.bindings.set(identifier.name, {
      ambiguous: false,
      init,
      kind,
      mutated: false,
      scope,
    });
  }

  function definePattern(scope, pattern, kind, init) {
    if (!pattern) return;
    nodeScopes.set(pattern, scope);
    if (pattern.type === "Identifier") {
      define(scope, pattern, kind, init);
      return;
    }
    if (pattern.type === "RestElement") {
      definePattern(scope, pattern.argument, kind, null);
      return;
    }
    if (pattern.type === "AssignmentPattern") {
      definePattern(scope, pattern.left, kind, null);
      visit(pattern.right, scope);
      return;
    }
    if (pattern.type === "ArrayPattern") {
      for (const element of pattern.elements) {
        definePattern(scope, element, kind, null);
      }
      return;
    }
    if (pattern.type === "ObjectPattern") {
      for (const property of pattern.properties) {
        nodeScopes.set(property, scope);
        if (property.type === "RestElement") {
          definePattern(scope, property.argument, kind, null);
        } else {
          visit(property.key, scope);
          definePattern(scope, property.value, kind, null);
        }
      }
    }
  }

  function visit(node, scope) {
    if (!isNode(node)) return;
    nodeScopes.set(node, scope);
    switch (node.type) {
      case "Program":
        for (const statement of node.body) visit(statement, scope);
        return;
      case "BlockStatement": {
        const blockScope = createScope(scope, "block");
        nodeScopes.set(node, blockScope);
        for (const statement of node.body) visit(statement, blockScope);
        return;
      }
      case "FunctionDeclaration": {
        if (node.id) define(scope, node.id, "function", null);
        const functionScope = createScope(scope, "function");
        for (const parameter of node.params) {
          definePattern(functionScope, parameter, "parameter", null);
        }
        visit(node.body, functionScope);
        return;
      }
      case "FunctionExpression":
      case "ArrowFunctionExpression": {
        const functionScope = createScope(scope, "function");
        if (node.type === "FunctionExpression" && node.id) {
          define(functionScope, node.id, "function", null);
        }
        for (const parameter of node.params) {
          definePattern(functionScope, parameter, "parameter", null);
        }
        visit(node.body, functionScope);
        return;
      }
      case "VariableDeclaration": {
        const declarationScope = node.kind === "var"
          ? nearestFunctionScope(scope)
          : scope;
        for (const declaration of node.declarations) {
          nodeScopes.set(declaration, scope);
          const directInit = declaration.id.type === "Identifier"
            ? declaration.init
            : null;
          definePattern(
            declarationScope,
            declaration.id,
            node.kind,
            directInit,
          );
          visit(declaration.init, scope);
        }
        return;
      }
      case "ClassDeclaration":
        if (node.id) define(scope, node.id, "class", null);
        break;
      case "ImportDeclaration":
        for (const specifier of node.specifiers) {
          nodeScopes.set(specifier, scope);
          define(scope, specifier.local, "import", null);
        }
        visit(node.source, scope);
        return;
      case "CatchClause": {
        const catchScope = createScope(scope, "block");
        definePattern(catchScope, node.param, "catch", null);
        visit(node.body, catchScope);
        return;
      }
      default:
        break;
    }
    forEachChild(node, child => visit(child, scope));
  }

  visit(ast, rootScope);
  walkAst(ast, node => {
    if (
      node.type === "AssignmentExpression" &&
      node.left.type === "Identifier"
    ) {
      const binding = resolveBinding(
        nodeScopes.get(node.left) || rootScope,
        node.left.name,
      );
      if (binding) binding.mutated = true;
    }
    if (
      node.type === "UpdateExpression" &&
      node.argument.type === "Identifier"
    ) {
      const binding = resolveBinding(
        nodeScopes.get(node.argument) || rootScope,
        node.argument.name,
      );
      if (binding) binding.mutated = true;
    }
  });
  return { bindingIdentifiers, nodeScopes, rootScope };
}

function resolveBinding(scope, name) {
  for (let current = scope; current; current = current.parent) {
    const binding = current.bindings.get(name);
    if (binding) return binding;
  }
  return null;
}

function evaluatePropertyNames(member, scope, model) {
  if (!member.computed) {
    return propertyNameFromSyntax(member.property);
  }
  return evaluateStaticStrings(member.property, scope, model);
}

function propertyNameFromSyntax(node) {
  if (node.type === "Identifier" || node.type === "PrivateIdentifier") {
    return new Set([node.name]);
  }
  if (node.type === "Literal" && typeof node.value === "string") {
    return new Set([node.value]);
  }
  return null;
}

function evaluateStaticStrings(node, scope, model, seen = new Set()) {
  const values = evaluateStaticValues(node, scope, model, seen);
  if (values === null) return null;
  const strings = new Set();
  for (const value of values) {
    const string = String(value);
    if (string.length > STATIC_STRING_LENGTH_LIMIT) return null;
    strings.add(string);
  }
  return strings.size <= STATIC_VALUE_LIMIT ? strings : null;
}

function evaluateStaticValues(node, scope, model, seen) {
  if (!node) return null;
  if (
    node.type === "Literal" &&
    ["string", "number", "boolean"].includes(typeof node.value)
  ) {
    return new Set([node.value]);
  }
  if (node.type === "Literal" && node.value === null) {
    return new Set([null]);
  }
  if (node.type === "Identifier") {
    const binding = resolveBinding(scope, node.name);
    if (
      !binding ||
      binding.ambiguous ||
      binding.kind !== "const" ||
      binding.mutated ||
      !binding.init ||
      seen.has(binding)
    ) {
      return null;
    }
    const nextSeen = new Set(seen);
    nextSeen.add(binding);
    return evaluateStaticValues(
      binding.init,
      model.nodeScopes.get(binding.init) || binding.scope,
      model,
      nextSeen,
    );
  }
  if (node.type === "TemplateLiteral") {
    let prefixes = new Set([node.quasis[0].value.cooked]);
    for (let index = 0; index < node.expressions.length; index++) {
      const expressionValues = evaluateStaticValues(
        node.expressions[index],
        model.nodeScopes.get(node.expressions[index]) || scope,
        model,
        seen,
      );
      if (expressionValues === null) return null;
      prefixes = combineStaticValues(
        prefixes,
        expressionValues,
        (left, right) => `${left}${String(right)}`,
      );
      if (prefixes === null) return null;
      prefixes = combineStaticValues(
        prefixes,
        new Set([node.quasis[index + 1].value.cooked]),
        (left, right) => `${left}${right}`,
      );
      if (prefixes === null) return null;
    }
    return prefixes;
  }
  if (node.type === "BinaryExpression" && node.operator === "+") {
    const left = evaluateStaticValues(
      node.left,
      model.nodeScopes.get(node.left) || scope,
      model,
      seen,
    );
    const right = evaluateStaticValues(
      node.right,
      model.nodeScopes.get(node.right) || scope,
      model,
      seen,
    );
    if (left === null || right === null) return null;
    return combineStaticValues(left, right, (a, b) => a + b);
  }
  if (node.type === "ConditionalExpression") {
    const consequent = evaluateStaticValues(
      node.consequent,
      model.nodeScopes.get(node.consequent) || scope,
      model,
      seen,
    );
    const alternate = evaluateStaticValues(
      node.alternate,
      model.nodeScopes.get(node.alternate) || scope,
      model,
      seen,
    );
    if (consequent === null || alternate === null) return null;
    const values = new Set([...consequent, ...alternate]);
    return values.size <= STATIC_VALUE_LIMIT ? values : null;
  }
  if (node.type === "SequenceExpression") {
    const finalExpression = node.expressions.at(-1);
    return evaluateStaticValues(
      finalExpression,
      model.nodeScopes.get(finalExpression) || scope,
      model,
      seen,
    );
  }
  if (
    node.type === "CallExpression" &&
    node.callee.type === "Identifier" &&
    node.callee.name === "String" &&
    resolveBinding(scope, "String") === null &&
    node.arguments.length === 1
  ) {
    const values = evaluateStaticValues(
      node.arguments[0],
      model.nodeScopes.get(node.arguments[0]) || scope,
      model,
      seen,
    );
    if (values === null) return null;
    return new Set([...values].map(value => String(value)));
  }
  if (
    node.type === "CallExpression" &&
    node.callee.type === "MemberExpression"
  ) {
    const methodNames = evaluatePropertyNames(node.callee, scope, model);
    if (methodNames?.size !== 1) return null;
    const methodName = [...methodNames][0];
    if (
      methodName === "join" &&
      node.callee.object.type === "ArrayExpression" &&
      node.arguments.length <= 1
    ) {
      const separators = node.arguments.length === 0
        ? new Set([","])
        : evaluateStaticStrings(
            node.arguments[0],
            model.nodeScopes.get(node.arguments[0]) || scope,
            model,
            seen,
          );
      if (separators === null) return null;
      let arrays = new Set([""]);
      for (let index = 0; index < node.callee.object.elements.length; index++) {
        const element = node.callee.object.elements[index];
        if (!element || element.type === "SpreadElement") return null;
        const elementValues = evaluateStaticValues(
          element,
          model.nodeScopes.get(element) || scope,
          model,
          seen,
        );
        if (elementValues === null) return null;
        const next = new Set();
        for (const prefix of arrays) {
          for (const value of elementValues) {
            for (const separator of separators) {
              const combined = index === 0
                ? String(value)
                : `${prefix}${separator}${String(value)}`;
              if (combined.length > STATIC_STRING_LENGTH_LIMIT) return null;
              next.add(combined);
              if (next.size > STATIC_VALUE_LIMIT) return null;
            }
          }
        }
        arrays = next;
      }
      return arrays;
    }
    if (methodName === "concat") {
      let values = evaluateStaticValues(
        node.callee.object,
        model.nodeScopes.get(node.callee.object) || scope,
        model,
        seen,
      );
      if (values === null) return null;
      for (const argument of node.arguments) {
        const argumentValues = evaluateStaticValues(
          argument,
          model.nodeScopes.get(argument) || scope,
          model,
          seen,
        );
        if (argumentValues === null) return null;
        values = combineStaticValues(
          values,
          argumentValues,
          (left, right) => `${String(left)}${String(right)}`,
        );
        if (values === null) return null;
      }
      return values;
    }
  }
  return null;
}

function combineStaticValues(left, right, combine) {
  const values = new Set();
  for (const leftValue of left) {
    for (const rightValue of right) {
      const value = combine(leftValue, rightValue);
      if (
        typeof value === "string" &&
        value.length > STATIC_STRING_LENGTH_LIMIT
      ) {
        return null;
      }
      values.add(value);
      if (values.size > STATIC_VALUE_LIMIT) return null;
    }
  }
  return values;
}

function globalObjectKind(node, scope, model, seen = new Set()) {
  if (node.type === "ChainExpression") {
    return globalObjectKind(node.expression, scope, model, seen);
  }
  if (node.type !== "Identifier") return null;
  const binding = resolveBinding(scope, node.name);
  if (!binding && GLOBAL_OBJECT_NAMES.has(node.name)) return node.name;
  if (
    !binding ||
    binding.ambiguous ||
    binding.kind !== "const" ||
    binding.mutated ||
    !binding.init ||
    seen.has(binding)
  ) {
    return null;
  }
  const nextSeen = new Set(seen);
  nextSeen.add(binding);
  return globalObjectKind(
    binding.init,
    model.nodeScopes.get(binding.init) || binding.scope,
    model,
    nextSeen,
  );
}

function isDynamicCallOrWrite(member, parent, key) {
  if (!member.computed || !parent) return false;
  if (
    parent.type === "AssignmentExpression" &&
    key === "left"
  ) {
    return !(
      parent.operator === "=" &&
      parent.right.type === "Literal" &&
      parent.right.value === null
    );
  }
  if (parent.type === "UpdateExpression") return true;
  return (
    (parent.type === "CallExpression" ||
      parent.type === "NewExpression") &&
    key === "callee"
  ) || (
    parent.type === "TaggedTemplateExpression" &&
    key === "tag"
  );
}

function checkReflectivePropertyOperation(
  node,
  scope,
  model,
  reportName,
  addIssue,
) {
  if (node.callee.type !== "MemberExpression") return;
  const owner = node.callee.object;
  if (owner.type !== "Identifier" || resolveBinding(scope, owner.name)) return;
  const methods = evaluatePropertyNames(node.callee, scope, model);
  if (methods?.size !== 1) return;
  const method = [...methods][0];
  const reflective =
    (owner.name === "Reflect" && ["get", "set"].includes(method)) ||
    (owner.name === "Object" && method === "defineProperty");
  if (!reflective || node.arguments.length < 2) return;
  const propertyNames = evaluateStaticStrings(
    node.arguments[1],
    model.nodeScopes.get(node.arguments[1]) || scope,
    model,
  );
  if (propertyNames !== null) {
    for (const propertyName of propertyNames) {
      reportName(node, propertyName);
    }
    return;
  }
  if (
    method !== "get" ||
    globalObjectKind(
      node.arguments[0],
      model.nodeScopes.get(node.arguments[0]) || scope,
      model,
    ) !== null
  ) {
    addIssue(node, "dynamic reflective property access is forbidden");
  }
}

function isReferenceIdentifier(node, parent, key, bindingIdentifiers) {
  if (bindingIdentifiers.has(node) || !parent) return false;
  if (
    parent.type === "MemberExpression" &&
    key === "property" &&
    !parent.computed
  ) {
    return false;
  }
  if (
    (parent.type === "Property" || parent.type === "MethodDefinition") &&
    key === "key" &&
    !parent.computed
  ) {
    return parent.type === "Property" &&
      parent.shorthand &&
      parent.value === node;
  }
  if (
    ["BreakStatement", "ContinueStatement", "LabeledStatement"].includes(
      parent.type,
    ) &&
    key === "label"
  ) {
    return false;
  }
  if (parent.type === "MetaProperty") return false;
  if (
    parent.type === "ExportSpecifier" &&
    key === "exported"
  ) {
    return false;
  }
  return true;
}

function walkAst(node, visit, parent = null, key = null) {
  if (!isNode(node)) return;
  visit(node, parent, key);
  for (const [childKey, value] of Object.entries(node)) {
    if (Array.isArray(value)) {
      for (const child of value) {
        if (isNode(child)) walkAst(child, visit, node, childKey);
      }
    } else if (isNode(value)) {
      walkAst(value, visit, node, childKey);
    }
  }
}

function forEachChild(node, visit) {
  for (const value of Object.values(node)) {
    if (Array.isArray(value)) {
      for (const child of value) {
        if (isNode(child)) visit(child);
      }
    } else if (isNode(value)) {
      visit(value);
    }
  }
}

function isNode(value) {
  return Boolean(
    value &&
    typeof value === "object" &&
    typeof value.type === "string",
  );
}
