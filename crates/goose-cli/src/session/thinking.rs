// Modified from ghosty (Apache-2.0); see NOTICE.
use rand::seq::IndexedRandom;

/// Frases que gira el indicador mientras el modelo piensa. Tono fantasma, corto,
/// sin punto final: el indicador les pega el `(Ctrl+C para interrumpir)`.
const THINKING_MESSAGES: &[&str] = &[
    "Atravesando paredes",
    "Apareciendo con una idea",
    "Flotando entre archivos",
    "Susurrando al compilador",
    "Invocando bytes",
    "Espantando bugs",
    "Materializando la respuesta",
    "Leyendo entre líneas",
    "Haciendo bu a los errores",
    "Vagando por el repo",
    "Asomándose al stack trace",
    "Levitando sobre el problema",
    "Recorriendo el árbol de directorios",
    "Desempolvando funciones",
    "Encendiendo velas de contexto",
    "Rastreando huellas en los logs",
    "Cruzando el umbral del módulo",
    "Poseyendo la terminal un momento",
    "Ordenando las ideas en la penumbra",
    "Escuchando lo que dice el código",
    "Cargando las cadenas de dependencias",
    "Recogiendo pistas digitales",
    "Buscando en la memoria",
    "Siguiendo el hilo de la conversación",
    "Comparando caminos posibles",
    "Armando la cadena de razonamiento",
    "Midiendo la coherencia de la respuesta",
    "Explorando el espacio de soluciones",
    "Procesando la intención del mensaje",
    "Trazando el grafo de pensamientos",
    "Evaluando rutas lógicas",
    "Revisando el contexto una vez más",
    "Afinando la sintaxis",
    "Tejiendo la respuesta",
    "Apagando la luz para pensar mejor",
    "Deslizándose por el historial",
    "Contando fantasmas en el heap",
    "Silbando en el pasillo del scheduler",
    "Reuniendo migajas de evidencia",
    "Volviendo del más allá con el resultado",
];

/// Una frase al azar de la lista.
pub fn get_random_thinking_message() -> &'static str {
    THINKING_MESSAGES
        .choose(&mut rand::rng())
        .unwrap_or(&THINKING_MESSAGES[0])
}
