// Example workflow demonstrating the shape: { name, description, fields[], run() }.
// Copy a workflow like this into ~/.config/herdr-launcher/workflows/ and edit run().

export default {
  name: "greet",
  description: "Example — prints a greeting (shows fields, defaults, run)",
  fields: [
    { name: "name", prompt: "Your name", required: true },
    { name: "lang", prompt: "Language (en/nl)", default: "en" },
  ],
  async run(values) {
    const hi = values.lang === "nl" ? "Hallo" : "Hello";
    console.log(`${hi}, ${values.name}!`);
  },
};
