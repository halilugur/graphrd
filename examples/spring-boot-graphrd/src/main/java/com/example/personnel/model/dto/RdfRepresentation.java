package com.example.personnel.model.dto;

/**
 * A resource (or a list) written out as RDF, with the media type that
 * matches the format — the service produces it, the controller only wraps
 * it into the HTTP answer.
 */
public record RdfRepresentation(String mediaType, String body) {
}
