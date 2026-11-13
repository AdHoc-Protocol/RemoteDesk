/** A stylesheet imported as its text ( build.mjs, the loader "text" ): the page puts it into a style of its own - one file, nothing beside it. */
declare module "*.css" {
    const text: string;
    export default text;
}
