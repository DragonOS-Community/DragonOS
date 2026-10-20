// Use a real Node entry point so imported programs can resolve process.argv[1].
import { pathToFileURL } from 'node:url';

const { sum } = await import(pathToFileURL(process.argv[2]));
console.log(JSON.stringify([sum([]), sum([-4, 1]), sum([2, 7, 11]), sum([-3, 0, 8, 12])]));
